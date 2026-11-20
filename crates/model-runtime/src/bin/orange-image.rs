//! Local SDXL host. Prompt/seed arrive on stdin, only a validated complete PNG is published.
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    local::run().await
}
#[cfg(not(unix))]
fn main() {
    eprintln!("No qualified SDXL image runtime for this target");
    std::process::exit(1);
}

#[cfg(unix)]
mod local {
    use serde::{Deserialize, Serialize};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::error::Error;
    use std::fs::File;
    use std::io::{BufReader as SyncReader, Cursor, Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::Stdio;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdout, Command};
    use tokio::signal::unix::{SignalKind, signal};

    const RUNNER: &str = include_str!("../../../../backends/orange_inference_backends/image.py");
    const PIN: &[u8] = include_bytes!("../../../../config/local-image.lock.json");
    const MODEL: &str = "sdxl-base-1.0-euler-fp16-mps-v1";
    const MAX_PNG: usize = 16 * 1024 * 1024;
    type Result<T> = std::result::Result<T, Box<dyn Error>>;

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        prompt: String,
        seed: u32,
    }
    #[derive(Debug, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Receipt {
        attempt_id: String,
        model_version: String,
        seed: u32,
        width: u32,
        height: u32,
        steps: u8,
        media_type: String,
        bytes: usize,
        sha256: String,
        rgb_sha256: String,
        observed_tensor_bytes: u64,
        observed_driver_bytes: u64,
    }
    #[derive(Deserialize)]
    #[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
    enum Event {
        Verifying {
            attempt_id: String,
        },
        Loading {
            attempt_id: String,
        },
        Denoising {
            attempt_id: String,
            completed_steps: u8,
        },
        Complete(Receipt),
    }

    async fn collect(
        child: &mut Child,
        stdout: ChildStdout,
        frame: &[u8],
        id: &str,
        seed: u32,
    ) -> Result<Receipt> {
        let mut input = child.stdin.take().ok_or("No private input pipe")?;
        input.write_all(frame).await?;
        drop(input);
        let mut output = BufReader::new(stdout);
        let mut sequence = 0u8;
        let mut complete = None;
        loop {
            let mut line = Vec::new();
            (&mut output)
                .take(4097)
                .read_until(b'\n', &mut line)
                .await?;
            if line.is_empty() {
                break;
            }
            if line.len() > 4096 || line.last() != Some(&b'\n') || complete.is_some() {
                return Err("Invalid image backend stream".into());
            }
            let value: Value = serde_json::from_slice(&line)?;
            if let Some(code) = value.get("error").and_then(Value::as_str) {
                // Give the interpreter time to close framework-owned semaphores on its normal error exit.
                let _ = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
                return Err(match code {
                    "invalid_image_request" => "Invalid image input or encoded prompt length",
                    "device_unavailable" => "Qualified MPS device unavailable",
                    "image_slot_busy" => "Image device slot busy",
                    _ => "Image backend unavailable",
                }
                .into());
            }
            let event: Event = serde_json::from_value(value)?;
            match event {
                Event::Verifying { attempt_id } if sequence == 0 && attempt_id == id => {}
                Event::Loading { attempt_id } if sequence == 1 && attempt_id == id => {}
                Event::Denoising {
                    attempt_id,
                    completed_steps,
                } if (2..=26).contains(&sequence)
                    && completed_steps == sequence - 1
                    && attempt_id == id => {}
                Event::Complete(r)
                    if sequence == 27
                        && r.attempt_id == id
                        && r.model_version == MODEL
                        && r.seed == seed
                        && (r.width, r.height, r.steps) == (1024, 1024, 25)
                        && r.media_type == "image/png"
                        && (1..=MAX_PNG).contains(&r.bytes)
                        && r.observed_tensor_bytes > 0
                        && r.observed_driver_bytes >= r.observed_tensor_bytes
                        && orange_protocol::valid_digest(&r.rgb_sha256)
                        && orange_protocol::valid_digest(&r.sha256) =>
                {
                    complete = Some(r);
                }
                _ => return Err("Unexpected image attempt or sequence".into()),
            }
            if complete.is_none() {
                std::io::stderr().write_all(&line)?;
            }
            sequence += 1;
        }
        if !child.wait().await?.success() {
            return Err("Image backend exited unsuccessfully".into());
        }
        complete.ok_or_else(|| "No complete image receipt".into())
    }

    fn verified_png(data: &[u8], receipt: &Receipt) -> Result<()> {
        if data.len() != receipt.bytes
            || hex::encode(Sha256::digest(data)) != receipt.sha256
            || !data.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            return Err("PNG identity mismatch".into());
        }
        let mut pos = 8;
        let mut count = 0;
        let mut end = false;
        while pos < data.len() {
            let header = data.get(pos..pos + 8).ok_or("Short PNG chunk")?;
            let size = u32::from_be_bytes(header[..4].try_into()?) as usize;
            match &header[4..] {
                b"IHDR" if pos == 8 && size == 13 => {}
                b"IDAT" if pos > 8 && !end => count += 1,
                b"IEND" if count > 0 && size == 0 && pos + 12 == data.len() => end = true,
                _ => return Err("Unexpected PNG chunk".into()),
            }
            pos = pos.checked_add(12 + size).ok_or("Oversized PNG chunk")?;
            if pos > data.len() {
                return Err("Truncated PNG chunk".into());
            }
        }
        if !end {
            return Err("Missing PNG end".into());
        }
        let mut reader = png::Decoder::new(SyncReader::new(Cursor::new(data))).read_info()?;
        let info = reader.info();
        if (info.width, info.height, info.color_type, info.bit_depth)
            != (1024, 1024, png::ColorType::Rgb, png::BitDepth::Eight)
        {
            return Err("PNG pixel format mismatch".into());
        }
        let mut pixels = vec![0; 1024 * 1024 * 3];
        let actual = reader.next_frame(&mut pixels)?;
        if actual.buffer_size() != pixels.len()
            || hex::encode(Sha256::digest(&pixels)) != receipt.rgb_sha256
            || pixels.chunks_exact(3).all(|pixel| pixel == &pixels[..3])
        {
            return Err("Decoded pixel verification failed".into());
        }
        reader.finish()?;
        Ok(())
    }

    pub async fn run() -> Result<()> {
        let args: Vec<_> = std::env::args_os().skip(1).map(PathBuf::from).collect();
        let [python, cache, slot, destination] = args.as_slice() else {
            return Err("Usage: orange-image <absolute venv python> <model cache> <private slot> <new PNG path>, JSON prompt and seed on stdin".into());
        };
        let metadata = slot.symlink_metadata()?;
        if !python.is_absolute()
            || !python.is_file()
            || !cache.is_absolute()
            || !slot.is_absolute()
            || !metadata.is_dir()
            || metadata.mode() & 0o077 != 0
            || destination.symlink_metadata().is_ok()
        {
            return Err("Invalid local paths or destination already exists".into());
        }
        let mut request = vec![];
        std::io::stdin().take(8193).read_to_end(&mut request)?;
        if request.len() > 8192 {
            return Err("Input exceeds request limit".into());
        }
        let Request { prompt, seed } = serde_json::from_slice(&request)?;
        if prompt.trim().is_empty() || prompt.len() > 2048 || prompt.contains('\0') {
            return Err("Invalid image prompt".into());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let frame = serde_json::to_vec(
            &json!({"protocol":1,"attempt_id":id,"model_version":MODEL,"prompt":prompt,"seed":seed}),
        )?;
        let temp = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .prefix(&format!("image-attempt-{id}-"))
            .tempdir_in(slot)?;
        let profile = temp.path().join("profile.json");
        std::fs::write(&profile, PIN)?;
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut child = Command::new(python)
            .args(["-I", "-B", "-c", RUNNER])
            .arg(profile)
            .arg(cache)
            .arg(slot)
            .arg(temp.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ORANGE_IMAGE_PARENT_PID", std::process::id().to_string())
            .env("HF_HOME", temp.path())
            .env("XDG_CACHE_HOME", temp.path())
            .env("TOKENIZERS_PARALLELISM", "false")
            .env("TORCH_HOME", temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or("No private output pipe")?;
        let outcome = tokio::select! {
            biased;
            _ = interrupt.recv() => Err("Image cancelled".into()),
            _ = term.recv() => Err("Image cancelled".into()),
            result = tokio::time::timeout(Duration::from_secs(1805),collect(&mut child,stdout,&frame,&id,seed)) => result.unwrap_or_else(|_| Err("Image deadline exceeded".into())),
        };
        if outcome.is_err() && child.try_wait()?.is_none() {
            child.kill().await?;
        }
        let receipt = outcome?;
        let file = File::options()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
            .open(temp.path().join("candidate.png"))?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() != receipt.bytes as u64 {
            return Err("Invalid candidate file".into());
        }
        let mut data = Vec::new();
        file.take(MAX_PNG as u64 + 1).read_to_end(&mut data)?;
        verified_png(&data, &receipt)?;
        let dir = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let mut staging = tempfile::NamedTempFile::new_in(dir)?;
        staging.write_all(&data)?;
        staging.as_file().sync_all()?;
        // A queued cancellation wins through the last point before atomic publication.
        tokio::select! {biased;_ = interrupt.recv()=>return Err("Image cancelled".into()),_ = term.recv()=>return Err("Image cancelled".into()),_ = std::future::ready(())=>{}}
        staging.persist_noclobber(destination)?;
        File::open(dir)?.sync_all()?;
        println!("{}", serde_json::to_string(&receipt)?);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn candidate_verification_reads_full_pixels_and_rejects_trailing_or_corrupt_data()
        -> Result<()> {
            let pixels: Vec<_> = (0..1024 * 1024 * 3).map(|i| (i % 251) as u8).collect();
            let mut data = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut data, 1024, 1024);
                encoder.set_color(png::ColorType::Rgb);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.write_header()?.write_image_data(&pixels)?;
            }
            let mut r = Receipt {
                attempt_id: uuid::Uuid::new_v4().to_string(),
                model_version: MODEL.into(),
                seed: 1,
                width: 1024,
                height: 1024,
                steps: 25,
                media_type: "image/png".into(),
                bytes: data.len(),
                sha256: hex::encode(Sha256::digest(&data)),
                rgb_sha256: hex::encode(Sha256::digest(&pixels)),
                observed_tensor_bytes: 1,
                observed_driver_bytes: 2,
            };
            verified_png(&data, &r)?;
            let mut event = serde_json::to_value(&r)?;
            event["phase"] = json!("complete");
            assert!(matches!(
                serde_json::from_value::<Event>(event)?,
                Event::Complete(_)
            ));
            r.rgb_sha256 = "0".repeat(64);
            assert!(verified_png(&data, &r).is_err());
            r.rgb_sha256 = hex::encode(Sha256::digest(&pixels));
            data.push(0);
            r.bytes = data.len();
            r.sha256 = hex::encode(Sha256::digest(&data));
            assert!(verified_png(&data, &r).is_err());
            data.pop();
            data[44] ^= 1;
            r.bytes = data.len();
            r.sha256 = hex::encode(Sha256::digest(&data));
            assert!(verified_png(&data, &r).is_err());
            Ok(())
        }
    }
}
