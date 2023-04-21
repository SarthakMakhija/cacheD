use std::hash::Hash;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::Receiver;

use crate::cache::command::{CommandStatus, CommandType};
use crate::cache::command::acknowledgement::CommandAcknowledgement;
use crate::cache::command::error::CommandSendError;
use crate::cache::expiration::TTLTicker;
use crate::cache::key_description::KeyDescription;
use crate::cache::policy::admission_policy::AdmissionPolicy;
use crate::cache::stats::ConcurrentStatsCounter;
use crate::cache::store::Store;

pub type CommandSendResult = Result<Arc<CommandAcknowledgement>, CommandSendError>;

pub(crate) struct CommandExecutor<Key, Value>
    where Key: Hash + Eq + Send + Sync + Clone + 'static,
          Value: Send + Sync + 'static {
    sender: crossbeam_channel::Sender<CommandAcknowledgementPair<Key, Value>>,
    keep_running: Arc<AtomicBool>,
}

struct CommandAcknowledgementPair<Key, Value>
    where Key: Hash + Eq + Clone {
    command: CommandType<Key, Value>,
    acknowledgement: Arc<CommandAcknowledgement>,
}

struct PutParameter<'a, Key, Value, DeleteHook>
    where Key: Hash + Eq + Send + Sync + Clone + 'static,
          Value: Send + Sync + 'static,
          DeleteHook: Fn(Key) {
    store: &'a Arc<Store<Key, Value>>,
    key_description: &'a KeyDescription<Key>,
    delete_hook: &'a DeleteHook,
    value: Value,
    admission_policy: &'a Arc<AdmissionPolicy<Key>>,
    stats_counter: &'a Arc<ConcurrentStatsCounter>,
}

struct PutWithTTLParameter<'a, Key, Value, DeleteHook>
    where Key: Hash + Eq + Send + Sync + Clone + 'static,
          Value: Send + Sync + 'static,
          DeleteHook: Fn(Key) {
    put_parameter: PutParameter<'a, Key, Value, DeleteHook>,
    ttl: Duration,
    ttl_ticker: &'a Arc<TTLTicker>,
}

struct DeleteParameter<'a, Key, Value>
    where Key: Hash + Eq + Send + Sync + Clone + 'static {
    store: &'a Arc<Store<Key, Value>>,
    key: &'a Key,
    admission_policy: &'a Arc<AdmissionPolicy<Key>>,
    ttl_ticker: &'a Arc<TTLTicker>,
}

struct UpdateTTLParameter<'a, Key, Value>
    where Key: Hash + Eq + Send + Sync + Clone + 'static {
    store: &'a Arc<Store<Key, Value>>,
    key: &'a Key,
    ttl: Duration,
    ttl_ticker: &'a Arc<TTLTicker>,
}

impl<Key, Value> CommandExecutor<Key, Value>
    where Key: Hash + Eq + Send + Sync + Clone + 'static,
          Value: Send + Sync + 'static {
    pub(crate) fn new(
        store: Arc<Store<Key, Value>>,
        admission_policy: Arc<AdmissionPolicy<Key>>,
        stats_counter: Arc<ConcurrentStatsCounter>,
        ttl_ticker: Arc<TTLTicker>,
        command_channel_size: usize) -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(command_channel_size);
        let command_executor = CommandExecutor { sender, keep_running: Arc::new(AtomicBool::new(true)) };

        command_executor.spin(receiver, store, admission_policy, stats_counter, ttl_ticker);
        command_executor
    }

    fn spin(&self,
            receiver: Receiver<CommandAcknowledgementPair<Key, Value>>,
            store: Arc<Store<Key, Value>>,
            admission_policy: Arc<AdmissionPolicy<Key>>,
            stats_counter: Arc<ConcurrentStatsCounter>,
            ttl_ticker: Arc<TTLTicker>) {
        let keep_running = self.keep_running.clone();
        let store_clone = store.clone();
        let delete_hook = move |key| { store_clone.delete(&key); };

        thread::spawn(move || {
            while let Ok(pair) = receiver.recv() {
                let command = pair.command;
                let status = match command {
                    CommandType::Put(key_description, value) =>
                        Self::put(PutParameter {
                            store: &store,
                            key_description: &key_description,
                            delete_hook: &delete_hook,
                            value,
                            admission_policy: &admission_policy,
                            stats_counter: &stats_counter,
                        }),
                    CommandType::PutWithTTL(key_description, value, ttl) =>
                        Self::put_with_ttl(PutWithTTLParameter {
                            put_parameter: PutParameter {
                                store: &store,
                                key_description: &key_description,
                                delete_hook: &delete_hook,
                                value,
                                admission_policy: &admission_policy,
                                stats_counter: &stats_counter,
                            },
                            ttl,
                            ttl_ticker: &ttl_ticker,
                        }),
                    CommandType::Delete(key) =>
                        Self::delete(DeleteParameter {
                            store: &store,
                            key: &key,
                            admission_policy: &admission_policy,
                            ttl_ticker: &ttl_ticker,
                        }),
                    CommandType::UpdateTTL(key, ttl) =>
                        Self::update_ttl(UpdateTTLParameter {
                            store: &store,
                            key: &key,
                            ttl,
                            ttl_ticker: &ttl_ticker,
                        }),
                };
                pair.acknowledgement.done(status);
                if !keep_running.load(Ordering::Acquire) {
                    drop(receiver);
                    break;
                }
            }
        });
    }

    pub(crate) fn send(&self, command: CommandType<Key, Value>) -> CommandSendResult {
        let acknowledgement = CommandAcknowledgement::new();
        let send_result = self.sender.send(CommandAcknowledgementPair {
            command,
            acknowledgement: acknowledgement.clone(),
        });

        match send_result {
            Ok(_) => Ok(acknowledgement),
            Err(err) => {
                println!("received a SendError while sending command type {}", err.0.command.description());
                Err(CommandSendError::new(err.0.command.description()))
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.keep_running.store(false, Ordering::Release);
    }

    fn put<DeleteHook>(put_parameters: PutParameter<Key, Value, DeleteHook>) -> CommandStatus where DeleteHook: Fn(Key) {
        let status = put_parameters.admission_policy.maybe_add(
            put_parameters.key_description,
            put_parameters.delete_hook,
        );
        if let CommandStatus::Accepted = status {
            put_parameters.store.put(
                put_parameters.key_description.clone_key(),
                put_parameters.value,
                put_parameters.key_description.id,
            );
        } else {
            put_parameters.stats_counter.reject_key();
        }
        status
    }

    fn put_with_ttl<DeleteHook>(put_with_ttl_parameter: PutWithTTLParameter<Key, Value, DeleteHook>) -> CommandStatus where DeleteHook: Fn(Key) {
        let status = put_with_ttl_parameter.put_parameter.admission_policy.maybe_add(
            put_with_ttl_parameter.put_parameter.key_description,
            put_with_ttl_parameter.put_parameter.delete_hook,
        );
        if let CommandStatus::Accepted = status {
            let expiry = put_with_ttl_parameter.put_parameter.store.put_with_ttl(
                put_with_ttl_parameter.put_parameter.key_description.clone_key(),
                put_with_ttl_parameter.put_parameter.value,
                put_with_ttl_parameter.put_parameter.key_description.id,
                put_with_ttl_parameter.ttl,
            );
            put_with_ttl_parameter.ttl_ticker.put(
                put_with_ttl_parameter.put_parameter.key_description.id,
                expiry,
            );
        } else {
            put_with_ttl_parameter.put_parameter.stats_counter.reject_key();
        }
        status
    }

    fn delete(delete_parameter: DeleteParameter<Key, Value>) -> CommandStatus {
        let may_be_key_id_expiry = delete_parameter.store.delete(delete_parameter.key);
        if let Some(key_id_expiry) = may_be_key_id_expiry {
            delete_parameter.admission_policy.delete(&key_id_expiry.0);
            if let Some(expiry) = key_id_expiry.1 {
                delete_parameter.ttl_ticker.delete(&key_id_expiry.0, &expiry);
            }
            return CommandStatus::Accepted;
        }
        CommandStatus::Rejected
    }

    fn update_ttl(update_ttl_parameter: UpdateTTLParameter<Key, Value>) -> CommandStatus {
        let response = update_ttl_parameter.store.update_time_to_live(update_ttl_parameter.key, update_ttl_parameter.ttl);
        if let Some(update_response) = response {
            match update_response.existing_expiry() {
                None =>
                    update_ttl_parameter.ttl_ticker.put(update_response.key_id(), update_response.new_expiry()),
                Some(existing_expiry) =>
                    update_ttl_parameter.ttl_ticker.update(update_response.key_id(), &existing_expiry, update_response.new_expiry())
            }
            return CommandStatus::Accepted;
        }
        CommandStatus::Rejected
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Add;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::cache::clock::{Clock, SystemClock};
    use crate::cache::command::{CommandStatus, CommandType};
    use crate::cache::command::command_executor::CommandExecutor;
    use crate::cache::command::command_executor::tests::setup::UnixEpochClock;
    use crate::cache::expiration::config::TTLConfig;
    use crate::cache::expiration::TTLTicker;
    use crate::cache::key_description::KeyDescription;
    use crate::cache::policy::admission_policy::AdmissionPolicy;
    use crate::cache::stats::ConcurrentStatsCounter;
    use crate::cache::store::Store;

    fn no_action_ttl_ticker() -> Arc<TTLTicker> {
        TTLTicker::new(TTLConfig::new(4, Duration::from_secs(300), SystemClock::boxed()), |_key_id| {})
    }

    mod setup {
        use std::time::SystemTime;

        use crate::cache::clock::Clock;

        #[derive(Clone)]
        pub(crate) struct UnixEpochClock;

        impl Clock for UnixEpochClock {
            fn now(&self) -> SystemTime {
                SystemTime::UNIX_EPOCH
            }
        }
    }

    #[tokio::test]
    async fn puts_a_key_value_and_shutdown() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );
        command_executor.shutdown();

        command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 10),
            "microservices",
        )).unwrap().handle().await;

        //introduce a delay to ensure that the thread in the spin method
        //loads the shutdown flag before the next command is sent
        thread::sleep(Duration::from_secs(1));

        let send_result = command_executor.send(CommandType::Put(
            KeyDescription::new("disk", 2, 2090, 10),
            "SSD",
        ));

        assert_eq!(Some("microservices"), store.get(&"topic"));
        assert_eq!(None, store.get(&"disk"));
        assert!(send_result.is_err())
    }

    #[tokio::test]
    async fn puts_a_key_value() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 10),
            "microservices",
        )).unwrap();
        command_acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(Some("microservices"), store.get(&"topic"));
    }

    #[tokio::test]
    async fn key_value_gets_rejected_given_its_weight_is_more_than_the_cache_weight() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter.clone(),
            no_action_ttl_ticker(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 200),
            "microservices",
        )).unwrap();
        let status = command_acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(None, store.get(&"topic"));
        assert_eq!(CommandStatus::Rejected, status);
    }

    #[tokio::test]
    async fn rejects_a_key_value_and_increase_stats() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter.clone(),
            no_action_ttl_ticker(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 200),
            "microservices",
        )).unwrap();
        let status = command_acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(CommandStatus::Rejected, status);
        assert_eq!(1, stats_counter.keys_rejected());
    }

    #[tokio::test]
    async fn puts_a_couple_of_key_values() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 10),
            "microservices",
        )).unwrap();
        let other_acknowledgment = command_executor.send(CommandType::Put(
            KeyDescription::new("disk", 2, 2076, 3),
            "SSD",
        )).unwrap();
        acknowledgement.handle().await;
        other_acknowledgment.handle().await;

        command_executor.shutdown();
        assert_eq!(Some("microservices"), store.get(&"topic"));
        assert_eq!(Some("SSD"), store.get(&"disk"));
    }

    #[tokio::test]
    async fn puts_a_key_value_with_ttl() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let ttl_ticker = no_action_ttl_ticker();
        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            ttl_ticker.clone(),
            10,
        );

        let acknowledgement = command_executor.send(CommandType::PutWithTTL(
            KeyDescription::new("topic", 1, 1029, 10),
            "microservices",
            Duration::from_secs(10),
        )).unwrap();
        acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(Some("microservices"), store.get(&"topic"));

        let expiry = store.get_ref(&"topic").unwrap().value().expire_after().unwrap();
        let expiry_in_ttl_ticker = ttl_ticker.get(&1, &expiry).unwrap();

        assert_eq!(expiry, expiry_in_ttl_ticker);
    }

    #[tokio::test]
    async fn rejects_a_key_value_with_ttl_and_increase_stats() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter.clone(),
            no_action_ttl_ticker(),
            10,
        );

        let acknowledgement = command_executor.send(CommandType::PutWithTTL(
            KeyDescription::new("topic", 1, 1029, 4000),
            "microservices",
            Duration::from_secs(10),
        )).unwrap();
        acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(1, stats_counter.keys_rejected());
    }

    #[tokio::test]
    async fn deletes_a_key() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));
        let ttl_ticker = no_action_ttl_ticker();

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            ttl_ticker.clone(),
            10,
        );

        let acknowledgement = command_executor.send(CommandType::PutWithTTL(
            KeyDescription::new("topic", 10, 1029, 10),
            "microservices",
            Duration::from_secs(10),
        )).unwrap();
        acknowledgement.handle().await;

        let expiry = store.get_ref(&"topic").unwrap().value().expire_after().unwrap();
        let expiry_in_ttl_ticker = ttl_ticker.get(&10, &expiry).unwrap();

        assert_eq!(Some("microservices"), store.get(&"topic"));
        assert_eq!(expiry, expiry_in_ttl_ticker);

        let acknowledgement =
            command_executor.send(CommandType::Delete("topic")).unwrap();
        acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(None, store.get(&"topic"));
        assert_eq!(None, ttl_ticker.get(&10, &expiry));
    }

    #[tokio::test]
    async fn deletion_of_a_non_existing_key_value_gets_rejected() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store: Arc<Store<&str, &str>> = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let acknowledgement =
            command_executor.send(CommandType::Delete("non-existing")).unwrap();
        let status = acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(CommandStatus::Rejected, status);
    }

    #[tokio::test]
    async fn updates_the_ttl_of_non_existing_key() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store: Arc<Store<&str, &str>> = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::UpdateTTL(
            "topic",
            Duration::from_secs(5),
        )).unwrap();
        let command_status = command_acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(CommandStatus::Rejected, command_status);
    }

    #[tokio::test]
    async fn updates_the_ttl_of_an_existing_key() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let clock = Box::new(UnixEpochClock {});
        let store: Arc<Store<&str, &str>> = Store::new(clock.clone(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));
        let ttl_ticker = no_action_ttl_ticker();

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            ttl_ticker.clone(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 50),
            "microservices",
        )).unwrap();
        let _ = command_acknowledgement.handle().await;

        let _ = command_executor.send(CommandType::UpdateTTL("topic", Duration::from_secs(5))).unwrap().handle().await;

        let key_value_ref = store.get_ref(&"topic").unwrap();

        let new_expiry = clock.now().add(Duration::from_secs(5));
        assert_eq!(Some(new_expiry), key_value_ref.value().expire_after());
        assert_eq!(Some(new_expiry), ttl_ticker.get(&1, &new_expiry));

        command_executor.shutdown();
    }

    #[tokio::test]
    async fn updates_the_ttl_of_an_existing_key_that_has_an_expiry() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let clock = Box::new(UnixEpochClock {});
        let store: Arc<Store<&str, &str>> = Store::new(clock.clone(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));
        let ttl_ticker = no_action_ttl_ticker();

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy,
            stats_counter,
            ttl_ticker.clone(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::PutWithTTL(
            KeyDescription::new("topic", 1, 1029, 50),
            "microservices",
            Duration::from_secs(25)
        )).unwrap();
        let _ = command_acknowledgement.handle().await;

        let _ = command_executor.send(CommandType::UpdateTTL("topic", Duration::from_secs(300))).unwrap().handle().await;

        let key_value_ref = store.get_ref(&"topic").unwrap();

        let new_expiry = clock.now().add(Duration::from_secs(300));
        assert_eq!(Some(new_expiry), key_value_ref.value().expire_after());
        assert_eq!(Some(new_expiry), ttl_ticker.get(&1, &new_expiry));

        command_executor.shutdown();
    }
}

#[cfg(test)]
mod sociable_tests {
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use crate::cache::clock::SystemClock;
    use crate::cache::command::{CommandStatus, CommandType};
    use crate::cache::command::command_executor::CommandExecutor;
    use crate::cache::expiration::config::TTLConfig;
    use crate::cache::expiration::TTLTicker;
    use crate::cache::key_description::KeyDescription;
    use crate::cache::policy::admission_policy::AdmissionPolicy;
    use crate::cache::pool::BufferConsumer;
    use crate::cache::stats::ConcurrentStatsCounter;
    use crate::cache::store::Store;

    fn no_action_ttl_ticker() -> Arc<TTLTicker> {
        TTLTicker::new(TTLConfig::new(4, Duration::from_secs(300), SystemClock::boxed()), |_key_id| {})
    }

    #[tokio::test]
    async fn puts_a_key_value() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy.clone(),
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let key_description = KeyDescription::new("topic", 1, 1029, 10);
        let key_id = key_description.id;
        let command_acknowledgement = command_executor.send(CommandType::Put(
            key_description,
            "microservices",
        )).unwrap();
        command_acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(Some("microservices"), store.get(&"topic"));
        assert!(admission_policy.contains(&key_id));
    }

    #[tokio::test]
    async fn puts_a_key_value_by_eliminating_victims() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 10, stats_counter.clone()));

        let key_hashes = vec![10, 14, 116];
        admission_policy.accept(key_hashes);
        thread::sleep(Duration::from_secs(1));

        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy.clone(),
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 10, 5),
            "microservices",
        )).unwrap();
        let status = command_acknowledgement.handle().await;
        assert_eq!(CommandStatus::Accepted, status);

        let command_acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("disk", 2, 14, 6),
            "SSD",
        )).unwrap();
        let status = command_acknowledgement.handle().await;
        assert_eq!(CommandStatus::Accepted, status);

        command_executor.shutdown();

        assert!(admission_policy.contains(&2));
        assert_eq!(Some("SSD"), store.get(&"disk"));

        assert!(!admission_policy.contains(&1));
        assert_eq!(None, store.get(&"topic"));
    }

    #[tokio::test]
    async fn deletes_a_key() {
        let stats_counter = Arc::new(ConcurrentStatsCounter::new());
        let store = Store::new(SystemClock::boxed(), stats_counter.clone());
        let admission_policy = Arc::new(AdmissionPolicy::new(10, 100, stats_counter.clone()));
        let command_executor = CommandExecutor::new(
            store.clone(),
            admission_policy.clone(),
            stats_counter,
            no_action_ttl_ticker(),
            10,
        );

        let acknowledgement = command_executor.send(CommandType::Put(
            KeyDescription::new("topic", 1, 1029, 10),
            "microservices",
        )).unwrap();
        acknowledgement.handle().await;

        let acknowledgement =
            command_executor.send(CommandType::Delete("topic")).unwrap();
        acknowledgement.handle().await;

        command_executor.shutdown();
        assert_eq!(None, store.get(&"topic"));
        assert!(!admission_policy.contains(&1));
    }
}