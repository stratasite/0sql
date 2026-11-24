//! The object store: where a result is kept for every node to read.
//!
//! One interface, the `object_store` crate's, over S3, Google Cloud
//! Storage, Azure and a directory. A result goes under `cache/`, which
//! leaves the rest of the store for whatever else is kept there.
//!
//! The store's options are handed over by name and never picked up from
//! the cloud's own environment variables: two datasource adapters fall
//! back to those, and a storage credential found there would quietly
//! become a warehouse login.

use crate::error::{Error, Result};
use futures_util::StreamExt;
use object_store::path::Path as Key;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, WriteMultipart};
use std::io::{Read, Write};
use std::path::Path;
use std::str::FromStr;

/// What every result's key starts with.
pub const PREFIX: &str = "cache";

/// A part of a large upload; a result smaller than one goes whole.
const PART: usize = 8 * 1024 * 1024;

/// The store results are kept in.
pub struct Objects {
    // A thread that is always there to drive the store's requests.
    runtime: Option<tokio::runtime::Runtime>,
    store: Box<dyn ObjectStore>,
    base: Key,
    /// The store as it was named, for an error to say.
    named: String,
}

impl Drop for Objects {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn failed(what: &str, e: impl std::fmt::Display) -> Error {
    Error::Objects(format!("{what}: {e}"))
}

impl Objects {
    /// The store a URL names (`s3://bucket/prefix`, `gs://`, `az://`,
    /// `file:///dir`), with its options by name. An option the store
    /// does not have is refused, not read past: a misspelled credential
    /// would otherwise be a store that fails later, somewhere else.
    pub fn open(url: &str, options: &[(String, String)]) -> Result<Objects> {
        let parsed = url::Url::parse(url).map_err(|e| failed(&format!("ZSQL_OBJECT_STORE {url:?}"), e))?;
        let known = |key: &str| match parsed.scheme() {
            "s3" | "s3a" => object_store::aws::AmazonS3ConfigKey::from_str(key).is_ok(),
            "gs" => object_store::gcp::GoogleConfigKey::from_str(key).is_ok(),
            "az" | "adl" | "azure" | "abfs" | "abfss" => object_store::azure::AzureConfigKey::from_str(key).is_ok(),
            "file" => false,
            // An https URL is one of the three, by its host.
            _ => {
                object_store::aws::AmazonS3ConfigKey::from_str(key).is_ok()
                    || object_store::gcp::GoogleConfigKey::from_str(key).is_ok()
                    || object_store::azure::AzureConfigKey::from_str(key).is_ok()
            }
        };
        if let Some((key, _)) = options.iter().find(|(key, _)| !known(key)) {
            return Err(Error::Objects(format!("ZSQL_OBJECT_STORE_{} is not an option of the store at {url}", key.to_uppercase())));
        }
        let (store, base) = object_store::parse_url_opts(&parsed, options.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .map_err(|e| failed(&format!("ZSQL_OBJECT_STORE {url:?}"), e))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("zsql-store")
            .enable_all()
            .build()
            .map_err(|e| failed("ZSQL_OBJECT_STORE", e))?;
        Ok(Objects { runtime: Some(runtime), store, base, named: url.to_string() })
    }

    /// The store as it was named.
    pub fn named(&self) -> &str {
        &self.named
    }

    /// Where a result's rows go: `cache/<id>/rows.arrow` under the
    /// store's own prefix.
    pub fn key_of(&self, id: &str) -> String {
        self.base.clone().join(PREFIX).join(id).join("rows.arrow").to_string()
    }

    fn on<T>(&self, work: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        let Some(runtime) = &self.runtime else { return Err(Error::Objects("the object store is closed".into())) };
        runtime.block_on(work)
    }

    /// Sends a result's file, whole or in parts, and says where it went.
    /// Nothing is at the key until all of it is.
    pub fn upload(&self, id: &str, file: &Path) -> Result<String> {
        let key = self.key_of(id);
        let at = Key::parse(&key).map_err(|e| failed(&key, e))?;
        let size = std::fs::metadata(file).map_err(|e| Error::Io(file.display().to_string(), e))?.len();
        let mut source = std::fs::File::open(file).map_err(|e| Error::Io(file.display().to_string(), e))?;
        self.on(async {
            if size <= PART as u64 {
                let mut bytes = Vec::with_capacity(size as usize);
                source.read_to_end(&mut bytes).map_err(|e| Error::Io(file.display().to_string(), e))?;
                self.store.put(&at, PutPayload::from(bytes)).await.map_err(|e| failed(&key, e))?;
                return Ok(());
            }
            let upload = self.store.put_multipart(&at).await.map_err(|e| failed(&key, e))?;
            let mut parts = WriteMultipart::new_with_chunk_size(upload, PART);
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                let read = source.read(&mut buffer).map_err(|e| Error::Io(file.display().to_string(), e))?;
                if read == 0 {
                    break;
                }
                // No more than a few parts in the air: the file is not
                // read faster than it is sent.
                parts.wait_for_capacity(4).await.map_err(|e| failed(&key, e))?;
                parts.write(&buffer[..read]);
            }
            parts.finish().await.map_err(|e| failed(&key, e))?;
            Ok(())
        })?;
        Ok(key)
    }

    /// Fetches a result's rows to `to`, written beside it and renamed,
    /// so a reader finds the whole file or none.
    pub fn download(&self, key: &str, to: &Path) -> Result<()> {
        let at = Key::parse(key).map_err(|e| failed(key, e))?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::Io(parent.display().to_string(), e))?;
        }
        // A file of this fetch's own: two readers of one result at once
        // each write a whole file, and the later rename replaces the
        // earlier with the same bytes.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = to.with_extension(format!("part-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let fetched = self.on(async {
            let mut body = self.store.get(&at).await.map_err(|e| failed(key, e))?.into_stream();
            let mut file = std::fs::File::create(&tmp).map_err(|e| Error::Io(tmp.display().to_string(), e))?;
            while let Some(piece) = body.next().await {
                file.write_all(&piece.map_err(|e| failed(key, e))?).map_err(|e| Error::Io(tmp.display().to_string(), e))?;
            }
            file.flush().map_err(|e| Error::Io(tmp.display().to_string(), e))
        });
        match fetched {
            Ok(()) => std::fs::rename(&tmp, to).map_err(|e| Error::Io(to.display().to_string(), e)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Removes a result's rows. One that is already gone is gone.
    pub fn delete(&self, key: &str) -> Result<()> {
        let at = Key::parse(key).map_err(|e| failed(key, e))?;
        self.on(async {
            match self.store.delete(&at).await {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(e) => Err(failed(key, e)),
            }
        })
    }
}
