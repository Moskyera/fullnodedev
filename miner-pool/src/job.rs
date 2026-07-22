use std::sync::RwLock;

use serde_json::Value as JV;

/// Latest mining job mirrored from the upstream fullnode.
#[derive(Debug, Clone, Default)]
pub struct MiningJob {
    pub height: u64,
    pub raw: JV,
    pub job_id: String,
}

pub struct JobHub {
    inner: RwLock<Option<MiningJob>>,
}

impl JobHub {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(None),
        }
    }

    pub fn update(&self, height: u64, raw: JV) {
        let job_id = format!("h{height}");
        let mut g = self.inner.write().expect("job hub write");
        *g = Some(MiningJob {
            height,
            raw,
            job_id,
        });
    }

    pub fn current(&self) -> Option<MiningJob> {
        self.inner.read().expect("job hub read").clone()
    }

    pub fn height(&self) -> u64 {
        self.current().map(|j| j.height).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_hub_updates_height_and_id() {
        let hub = JobHub::new();
        assert_eq!(hub.height(), 0);
        hub.update(100, json!({"height": 100, "block_intro": "aa"}));
        let j = hub.current().unwrap();
        assert_eq!(j.height, 100);
        assert_eq!(j.job_id, "h100");
    }
}
