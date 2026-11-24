use super::*;
#[derive(Clone)]
pub(super) struct Transport {
    pub(super) client: reqwest::Client,
    pub(super) origin: String,
}

impl HttpTransport for Transport {
    async fn fetch(&self, req: Request<Buffer>) -> opendal::Result<Response<HttpBody>> {
        let invalid = || {
            opendal::Error::new(
                opendal::ErrorKind::Unexpected,
                "artifact provider transport failed",
            )
        };
        let (parts, body) = req.into_parts();
        let url = reqwest::Url::parse(&parts.uri.to_string()).map_err(|_| invalid())?;
        if url.origin().ascii_serialization() != self.origin
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid());
        }
        let head = parts.method == Method::HEAD;
        let mut reply = self
            .client
            .request(parts.method, url)
            .headers(parts.headers)
            .body(body.to_bytes())
            .send()
            .await
            .map_err(|_| invalid())?;
        let mut response = Response::builder().status(reply.status());
        for (k, v) in reply.headers() {
            response = response.header(k, v);
        }
        let mut bytes = Vec::new();
        if !head {
            let limit = orange_protocol::artifacts::OBJECT_LIMIT as usize;
            if reply.content_length().is_some_and(|n| n > limit as u64) {
                return Err(invalid());
            }
            while let Some(chunk) = reply.chunk().await.map_err(|_| invalid())? {
                if chunk.len() > limit - bytes.len() {
                    return Err(invalid());
                }
                bytes.extend_from_slice(&chunk);
            }
        }
        let n = bytes.len() as u64;
        response
            .body(HttpBody::new(
                tokio_stream::once(Ok(Buffer::from(bytes))),
                Some(n),
            ))
            .map_err(|_| invalid())
    }
}
