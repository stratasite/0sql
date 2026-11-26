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

/// How long one put of [`Objects::put_new`] may take.
pub const PUT_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

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

    /// Puts `bytes` at `path` beneath the store's own prefix, whole,
    /// where nothing is yet, and says where they went: the key
    /// [`Objects::delete`] takes, or `None` when something is there
    /// already, which is left as it was. A store that cannot be asked
    /// for that is simply put to. A put that has not ended in
    /// `PUT_WITHIN` is given up: its caller is holding something. A put
    /// that failed or was given up is cleared away as well as it can
    /// be, since it may have landed all the same.
    /// For what is kept beside the results and is not one: nothing here
    /// ever clears it away.
    pub fn put_new(&self, path: &[&str], bytes: Vec<u8>) -> Result<Option<String>> {
        let key = path.iter().fold(self.base.clone(), |at, part| at.join(*part)).to_string();
        let at = Key::parse(&key).map_err(|e| failed(&key, e))?;
        let payload = PutPayload::from(bytes);
        self.on(async {
            let putting = async {
                match self.store.put_opts(&at, payload.clone(), object_store::PutMode::Create.into()).await {
                    Ok(_) => Ok(true),
                    Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
                    Err(object_store::Error::NotImplemented { .. }) => self.store.put(&at, payload).await.map(|_| true),
                    Err(e) => Err(e),
                }
            };
            let put = match tokio::time::timeout(PUT_WITHIN, putting).await {
                Ok(done) => done.map_err(|e| failed(&key, e)),
                Err(_) => Err(failed(&key, format!("not put within {} seconds", PUT_WITHIN.as_secs()))),
            };
            if put.is_err() {
                // The store may have taken the bytes and the answer been
                // lost, or the put been given up as it landed: the caller
                // is told nothing was put, so nothing is left there.
                let _ = tokio::time::timeout(PUT_WITHIN, self.store.delete(&at)).await;
            }
            put
        })
        .map(|put| put.then_some(key))
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

    fn path_key(&self, path: &[&str]) -> String {
        path.iter().fold(self.base.clone(), |at, part| at.join(*part)).to_string()
    }

    /// Puts `bytes` at `path` beneath the store's own prefix, over
    /// whatever is there: for what is kept beside the results and is
    /// changed in place, as a dataset's backup is. Given up after
    /// `PUT_WITHIN`.
    pub fn put(&self, path: &[&str], bytes: Vec<u8>) -> Result<String> {
        let key = self.path_key(path);
        let at = Key::parse(&key).map_err(|e| failed(&key, e))?;
        self.on(async {
            match tokio::time::timeout(PUT_WITHIN, self.store.put(&at, PutPayload::from(bytes))).await {
                Ok(done) => done.map(|_| ()).map_err(|e| failed(&key, e)),
                Err(_) => Err(failed(&key, format!("not put within {} seconds", PUT_WITHIN.as_secs()))),
            }
        })?;
        Ok(key)
    }

    /// What is at `path` beneath the store's own prefix.
    pub fn get(&self, path: &[&str]) -> Result<Vec<u8>> {
        let key = self.path_key(path);
        let at = Key::parse(&key).map_err(|e| failed(&key, e))?;
        self.on(async {
            let got = self.store.get(&at).await.map_err(|e| failed(&key, e))?;
            got.bytes().await.map(|b| b.to_vec()).map_err(|e| failed(&key, e))
        })
    }

    /// Everything beneath `path`, each as its parts below it.
    pub fn list(&self, path: &[&str]) -> Result<Vec<Vec<String>>> {
        let key = self.path_key(path);
        let at = Key::parse(&key).map_err(|e| failed(&key, e))?;
        let depth = at.parts().count();
        self.on(async {
            let mut out = Vec::new();
            let mut listing = self.store.list(Some(&at));
            while let Some(meta) = listing.next().await {
                let meta = meta.map_err(|e| failed(&key, e))?;
                out.push(meta.location.parts().skip(depth).map(|p| p.as_ref().to_string()).collect());
            }
            Ok(out)
        })
    }

    /// Removes everything beneath `path`; how many it removed.
    pub fn delete_under(&self, path: &[&str]) -> Result<usize> {
        let below = self.list(path)?;
        for rest in &below {
            let mut full: Vec<&str> = path.to_vec();
            full.extend(rest.iter().map(String::as_str));
            self.delete(&self.path_key(&full))?;
        }
        Ok(below.len())
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
