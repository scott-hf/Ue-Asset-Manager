//! Lazy thumbnail loading with an in-memory texture cache.
//!
//! Rows ask for a thumbnail when they are bound (`want`) and give it up when they scroll away
//! (`unwant`); a job that is still queued for a row that is gone is skipped. The job itself is
//! a closure that returns PNG/JPEG bytes (for the Assets page: `ue_asset_library::read_thumbnail`)
//! and runs on a small thread pool. Decoded textures are cached per key for the session.

use gtk4::{gdk, glib};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const THREADS: usize = 3;

type Finished = (String, Option<Option<Vec<u8>>>);

pub struct ThumbLoader {
    pool: threadpool::ThreadPool,
    wanted: Arc<Mutex<HashSet<String>>>,
    pending: RefCell<HashSet<String>>,
    textures: RefCell<HashMap<String, Option<gdk::Texture>>>,
    sender: async_channel::Sender<Finished>,
}

impl ThumbLoader {
    /// `on_ready` runs on the main thread after every finished (or skipped) job.
    pub fn new(on_ready: impl Fn() + 'static) -> Rc<Self> {
        let (sender, receiver) = async_channel::unbounded::<Finished>();
        let loader = Rc::new(Self {
            pool: threadpool::ThreadPool::new(THREADS),
            wanted: Arc::new(Mutex::new(HashSet::new())),
            pending: RefCell::new(HashSet::new()),
            textures: RefCell::new(HashMap::new()),
            sender,
        });
        let weak = Rc::downgrade(&loader);
        glib::spawn_future_local(async move {
            while let Ok((key, result)) = receiver.recv().await {
                let Some(loader) = weak.upgrade() else {
                    break;
                };
                loader.finish(&key, result);
                on_ready();
            }
        });
        loader
    }

    fn finish(&self, key: &str, result: Option<Option<Vec<u8>>>) {
        self.pending.borrow_mut().remove(key);
        if let Some(bytes) = result {
            let texture = bytes.and_then(|b| {
                gdk::Texture::from_bytes(&glib::Bytes::from_owned(b))
                    .map_err(|e| log::debug!("Thumbnail {key} is not decodable: {e}"))
                    .ok()
            });
            self.textures.borrow_mut().insert(key.to_string(), texture);
        }
    }

    /// `Some(Some(texture))` loaded, `Some(None)` there is none (use an icon), `None` unknown yet.
    pub fn texture(&self, key: &str) -> Option<Option<gdk::Texture>> {
        self.textures.borrow().get(key).cloned()
    }

    /// Start loading `key` (once) with `job`, which returns the image bytes.
    pub fn want(&self, key: &str, job: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
        if let Ok(mut wanted) = self.wanted.lock() {
            wanted.insert(key.to_string());
        }
        if self.textures.borrow().contains_key(key) || !self.pending.borrow_mut().insert(key.into())
        {
            return;
        }
        let wanted = self.wanted.clone();
        let sender = self.sender.clone();
        let key = key.to_string();
        self.pool.execute(move || {
            let still_wanted = wanted.lock().map(|w| w.contains(&key)).unwrap_or(true);
            let result = still_wanted.then(job);
            let _ = sender.send_blocking((key, result));
        });
    }

    /// The row for `key` is gone; a job that has not started yet is skipped.
    pub fn unwant(&self, key: &str) {
        if let Ok(mut wanted) = self.wanted.lock() {
            wanted.remove(key);
        }
    }
}
