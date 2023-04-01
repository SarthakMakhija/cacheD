use std::ops::Add;
use std::time::{Duration, SystemTime};

use crate::cache::clock::ClockType;
use crate::cache::types::KeyId;

pub struct StoredValue<Value> {
    value: Value,
    key_id: KeyId,
    expire_after: Option<SystemTime>,
}

impl<Value> StoredValue<Value> {
    pub(crate) fn never_expiring(value: Value, key_id: KeyId) -> Self {
        StoredValue {
            value,
            key_id,
            expire_after: None,
        }
    }

    pub(crate) fn expiring(value: Value, key_id: KeyId, time_to_live: Duration, clock: &ClockType) -> Self {
        StoredValue {
            value,
            key_id,
            expire_after: Some(clock.now().add(time_to_live)),
        }
    }

    pub(crate) fn is_alive(&self, clock: &ClockType) -> bool {
        if let Some(expire_after) = self.expire_after {
            return !clock.has_passed(&expire_after);
        }
        true
    }

    pub fn value_ref(&self) -> &Value {
        &self.value
    }

    pub fn key_id(&self) -> KeyId {
        self.key_id
    }
}

impl<Value> StoredValue<Value>
    where Value: Clone {

    pub fn value(&self) -> Value {
        self.value.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Add;
    use std::time::{Duration, SystemTime};

    use crate::cache::clock::{ClockType, SystemClock};
    use crate::cache::store::stored_value::StoredValue;
    use crate::cache::store::stored_value::tests::setup::{FutureClock, UnixEpochClock};

    mod setup {
        use std::ops::Add;
        use std::time::{Duration, SystemTime};

        use crate::cache::clock::Clock;

        #[derive(Clone)]
        pub(crate) struct FutureClock;

        #[derive(Clone)]
        pub(crate) struct UnixEpochClock;

        impl Clock for FutureClock {
            fn now(&self) -> SystemTime {
                SystemTime::now().add(Duration::from_secs(10))
            }
        }

        impl Clock for UnixEpochClock {
            fn now(&self) -> SystemTime {
                SystemTime::UNIX_EPOCH
            }
        }
    }

    #[test]
    fn expiration_time() {
        let clock: ClockType = Box::new(UnixEpochClock {});
        let stored_value = StoredValue::expiring("SSD", 1, Duration::from_secs(10), &clock);

        assert!(stored_value.expire_after.unwrap().eq(&SystemTime::UNIX_EPOCH.add(Duration::from_secs(10))));
    }

    #[test]
    fn is_alive() {
        let stored_value = StoredValue::never_expiring("storage-engine", 1);

        assert!(stored_value.is_alive(&SystemClock::boxed()));
    }

    #[test]
    fn is_not_alive() {
        let system_clock = SystemClock::boxed();
        let stored_value = StoredValue::expiring("storage-engine", 1, Duration::from_secs(5), &system_clock);

        let future_clock: ClockType = Box::new(FutureClock {});
        assert!(!stored_value.is_alive(&future_clock));
    }
}