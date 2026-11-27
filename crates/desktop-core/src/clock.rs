use chrono::{DateTime, Duration, Utc};

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[cfg(any(test, feature = "testing"))]
#[derive(Debug)]
pub struct MockClock {
    current: std::sync::Mutex<DateTime<Utc>>,
}

#[cfg(any(test, feature = "testing"))]
impl MockClock {
    #[must_use]
    pub fn new(start: DateTime<Utc>) -> Self {
        Self {
            current: std::sync::Mutex::new(start),
        }
    }

    pub fn advance(&self, delta: Duration) {
        let mut guard = self.current.lock().expect("mock clock poisoned");
        *guard += delta;
    }
}

#[cfg(any(test, feature = "testing"))]
impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        *self.current.lock().expect("mock clock poisoned")
    }
}

pub fn default_lease_ttl() -> Duration {
    Duration::minutes(10)
}
