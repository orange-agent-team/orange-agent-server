//! Owned-file diagnostic; contains no user paths, attribute values or file contents.
use std::{fs, io, time::Duration};
fn main() -> io::Result<()> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    let target = root.path().join("target");
    fs::write(&source, b"owned original")?;
    std::thread::sleep(Duration::from_millis(1200));
    fs::write(&target, b"owned replacement")?;
    let a = fs::File::open(&source)?;
    let b = fs::File::options().read(true).write(true).open(&target)?;
    let before = orange_platform_files::security_metadata(&a)?;
    let result = orange_platform_files::copy_security_metadata(&a, &b);
    match &result {
        Ok(_) => println!("copy=ok"),
        Err(e) => println!(
            "copy=error kind={:?} errno={:?} detail={e}",
            e.kind(),
            e.raw_os_error()
        ),
    }
    println!(
        "source_unchanged={}",
        orange_platform_files::security_metadata(&a)? == before
    );
    println!(
        "metadata_equal={}",
        orange_platform_files::security_metadata(&b)? == before
    );
    println!(
        "source={before:?} target={:?}",
        orange_platform_files::security_metadata(&b)?
    );
    let mut names = vec![0; 16384];
    let count = rustix::fs::flistxattr(&a, &mut names)?;
    for name in names[..count].split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let mut av = vec![0; 65536];
        let mut bv = vec![0; 65536];
        let ac = rustix::fs::fgetxattr(&a, name, &mut av)?;
        let bc = rustix::fs::fgetxattr(&b, name, &mut bv);
        println!(
            "attribute={} equal={}",
            String::from_utf8_lossy(name),
            bc.is_ok_and(|n| av[..ac] == bv[..n])
        );
        if name == b"com.apple.quarantine" {
            let fields: Vec<_> = av[..ac].split(|b| *b == b';').collect();
            if let Ok(n) = bc {
                let target_fields: Vec<_> = bv[..n].split(|b| *b == b';').collect();
                println!(
                    "quarantine_fields_equal={:?}",
                    fields
                        .iter()
                        .zip(target_fields)
                        .map(|(a, b)| *a == b)
                        .collect::<Vec<_>>()
                );
            }
        }
    }
    println!(
        "contents_preserved={}",
        fs::read(&source)? == b"owned original" && fs::read(&target)? == b"owned replacement"
    );
    let published = orange_platform_files::copy_publication_metadata(&a, &b)?;
    assert_eq!(orange_platform_files::security_metadata(&a)?, before);
    assert_eq!(orange_platform_files::security_metadata(&b)?, published);
    assert_eq!(fs::read(&source)?, b"owned original");
    assert_eq!(fs::read(&target)?, b"owned replacement");
    println!("publication_policy=passed exact_target_bound=true source_unchanged=true");
    Ok(())
}
