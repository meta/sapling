// (c) Meta Platforms, Inc. and affiliates. Confidential and proprietary.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use fbinit::FacebookInit;
use metaconfig_parser::RepoConfigs;
use metaconfig_parser::StorageConfigs;
use metaconfig_types::CommonConfig;
use metaconfig_types::MetadataCacheConfig;
use metaconfig_types::MetadataCacheUpdateMode;
use mononoke_configs::MononokeConfigs;
use mononoke_macros::mononoke;
use repo_event_publisher::AllBookmarksFilter;
use repo_event_publisher::RepoEventPublisher;
use repo_event_publisher::ScribeListener;
use repo_event_publisher::ScribeRepoEventPublisher;
use repo_update_logger::PlainBookmarkInfo;
use scribe_api::ReadFlavor;
use scribe_api::consumer::ScribeConsumerOptions;
use scribe_api::consumer::SerializationFormat;
use scribe_api::inmemory::create_consumer_producer_pair;
use scribe_api::producer::ScribeProducer;
use scribe_api::producer::ScribeProducerOptions;

fn mononoke_configs() -> Arc<MononokeConfigs> {
    Arc::new(MononokeConfigs::new_static(
        RepoConfigs::new(HashMap::new(), CommonConfig::default()),
        StorageConfigs {
            storage: HashMap::new(),
        },
    ))
}

#[mononoke::fbinit_test]
async fn subscribes_to_eventrouter_bookmark_notifications(fb: FacebookInit) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY", "log");
    }
    let metadata_cache_config = MetadataCacheConfig {
        wbc_event_router_update_mode: Some(MetadataCacheUpdateMode::Tailing {
            category: "event_router_bookmark_events".to_owned(),
        }),
        ..Default::default()
    };
    let publisher =
        ScribeRepoEventPublisher::new(fb, mononoke_configs(), Some(&metadata_cache_config))?;

    let _receiver =
        publisher.subscribe_for_bookmark_event_notifications(&"test_repo".to_owned())?;
    assert!(
        publisher
            .subscribe_for_bookmark_updates(&"test_repo".to_owned())
            .is_err(),
        "EventRouter configuration should not enable legacy bookmark notifications"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn missing_eventrouter_stream_returns_error(fb: FacebookInit) -> anyhow::Result<()> {
    let publisher = ScribeRepoEventPublisher::new(fb, mononoke_configs(), None)?;
    assert!(
        publisher
            .subscribe_for_bookmark_event_notifications(&"test_repo".to_owned())
            .is_err(),
        "a publisher without an EventRouter stream has nothing to subscribe to"
    );
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_basic_listener_creation(fb: FacebookInit) -> anyhow::Result<()> {
    ScribeListener::<PlainBookmarkInfo, _>::new(
        fb,
        mononoke_configs(),
        "test_category",
        AllBookmarksFilter {},
    )
    .expect("Failed to create scribe listener");
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_listener_with_single_subscriber(fb: FacebookInit) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY", "log");
    }
    let producer_options = ScribeProducerOptions::builder()
        .name("test_producer")
        .build();
    let consumer_options = ScribeConsumerOptions::builder()
        .backlog(100)
        .serialization_format(SerializationFormat::SCUBA_JSON)
        .maybe_read_flavor(Some(ReadFlavor::HIGH_DURABILITY))
        .build();
    let (consumer, producer) =
        create_consumer_producer_pair(fb, "test_category", consumer_options, producer_options)?;

    let bookmark_info_json = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    let consumer = Arc::new(consumer);
    let listener = ScribeListener::<PlainBookmarkInfo, _>::new_with_client(
        fb,
        mononoke_configs(),
        consumer.clone(),
        "test_category".to_string(),
        AllBookmarksFilter {},
    )
    .expect("Failed to create scribe listener");
    let mut receiver = listener.subscribe(&"test_repo".to_string());
    producer.write_message(&mut Cursor::new(bookmark_info_json.as_bytes()), None)?;
    let notification_bookmark_info = receiver.recv().await.expect("Failed to receive message");
    assert_eq!(
        notification_bookmark_info.bookmark_name,
        "test_bookmark".to_string()
    );
    // Send multiple messages and ensure that the subscriber gets all of them
    let message_1 = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "new_bookmark",
                "new_bookmark_value": "new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "create",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    let message_2 = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "deleted_bookmark",
                "old_bookmark_value": "test_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "delete",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    let message_3 = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    producer.write_message(&mut Cursor::new(message_1.as_bytes()), None)?;
    producer.write_message(&mut Cursor::new(message_2.as_bytes()), None)?;
    producer.write_message(&mut Cursor::new(message_3.as_bytes()), None)?;
    // Ensure all three messages are received in the expected order
    let received_msg_1 = receiver.recv().await?;
    let received_msg_2 = receiver.recv().await?;
    let received_msg_3 = receiver.recv().await?;
    assert_eq!(received_msg_1.bookmark_name, "new_bookmark".to_string());
    assert_eq!(received_msg_2.bookmark_name, "deleted_bookmark".to_string());
    assert_eq!(received_msg_3.bookmark_name, "test_bookmark".to_string());
    Ok(())
}

#[mononoke::fbinit_test]
async fn test_listener_with_multiple_subscriber_same_repo(fb: FacebookInit) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY", "log");
    }
    let producer_options = ScribeProducerOptions::builder()
        .name("test_producer")
        .build();
    let consumer_options = ScribeConsumerOptions::builder()
        .backlog(100)
        .serialization_format(SerializationFormat::SCUBA_JSON)
        .build();
    let (consumer, producer) =
        create_consumer_producer_pair(fb, "test_category", consumer_options, producer_options)?;

    let bookmark_info_json = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    let listener = ScribeListener::<PlainBookmarkInfo, _>::new_with_client(
        fb,
        mononoke_configs(),
        Arc::new(consumer),
        "test_category".to_string(),
        AllBookmarksFilter {},
    )
    .expect("Failed to create scribe listener");
    let mut receiver_1 = listener.subscribe(&"test_repo".to_string());
    let mut receiver_2 = listener.subscribe(&"test_repo".to_string());
    let mut receiver_3 = listener.subscribe(&"test_repo".to_string());
    producer.write_message(&mut Cursor::new(bookmark_info_json.as_bytes()), None)?;
    // Ensure that all subscribers get the expected notification
    let notification_1 = receiver_1.recv().await?;
    let notification_2 = receiver_2.recv().await?;
    let notification_3 = receiver_3.recv().await?;
    assert_eq!(notification_1.bookmark_name, "test_bookmark".to_string());
    assert_eq!(notification_2.bookmark_name, "test_bookmark".to_string());
    assert_eq!(notification_3.bookmark_name, "test_bookmark".to_string());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_listener_with_multiple_subscriber_multiple_repo(
    fb: FacebookInit,
) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY", "log");
    }
    let producer_options = ScribeProducerOptions::builder()
        .name("test_producer")
        .build();
    let consumer_options = ScribeConsumerOptions::builder()
        .backlog(100)
        .serialization_format(SerializationFormat::SCUBA_JSON)
        .build();
    let (consumer, producer) =
        create_consumer_producer_pair(fb, "test_category", consumer_options, producer_options)?;

    let test_repo_bookmark = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    let listener = ScribeListener::<PlainBookmarkInfo, _>::new_with_client(
        fb,
        mononoke_configs(),
        Arc::new(consumer),
        "test_category".to_string(),
        AllBookmarksFilter {},
    )
    .expect("Failed to create scribe listener");
    let mut receiver_1 = listener.subscribe(&"test_repo".to_string());
    let mut receiver_2 = listener.subscribe(&"other_repo".to_string());
    let mut receiver_3 = listener.subscribe(&"another_repo".to_string());
    producer.write_message(&mut Cursor::new(test_repo_bookmark.as_bytes()), None)?;
    let other_repo_bookmark = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "other_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    producer.write_message(&mut Cursor::new(other_repo_bookmark.as_bytes()), None)?;
    let another_repo_bookmark = r#"
        {
            "int": {
                "time": 1744712410        
            },
            "normal": {
                "repo_name": "another_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "test_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    producer.write_message(&mut Cursor::new(another_repo_bookmark.as_bytes()), None)?;
    // Ensure that all subscribers get the expected notification
    let test_repo_bookmark = receiver_1.recv().await?;
    let other_repo_bookmark = receiver_2.recv().await?;
    let another_repo_bookmark = receiver_3.recv().await?;
    assert_eq!(test_repo_bookmark.repo_name, "test_repo".to_string());
    assert_eq!(other_repo_bookmark.repo_name, "other_repo".to_string());
    assert_eq!(another_repo_bookmark.repo_name, "another_repo".to_string());

    Ok(())
}

#[mononoke::fbinit_test]
async fn test_no_reads_before_first_subscribe(fb: FacebookInit) -> anyhow::Result<()> {
    unsafe {
        std::env::set_var("MONONOKE_TEST_SCRIBE_LOGGING_DIRECTORY", "log");
    }
    let producer_options = ScribeProducerOptions::builder()
        .name("test_producer")
        .build();
    let consumer_options = ScribeConsumerOptions::builder()
        .backlog(100)
        .serialization_format(SerializationFormat::SCUBA_JSON)
        .build();
    let (consumer, producer) =
        create_consumer_producer_pair(fb, "test_category", consumer_options, producer_options)?;

    let listener = ScribeListener::<PlainBookmarkInfo, _>::new_with_client(
        fb,
        mononoke_configs(),
        Arc::new(consumer),
        "test_category".to_string(),
        AllBookmarksFilter {},
    )?;

    // Written before any subscription exists. A read loop running from
    // construction would consume this while no sender is registered and drop
    // it; with lazy start it stays buffered until the loop starts.
    let first_bookmark_json = r#"
        {
            "int": {
                "time": 1744712410
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "first_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    producer.write_message(&mut Cursor::new(first_bookmark_json.as_bytes()), None)?;
    // Give a read loop running from construction time to consume (and drop)
    // the message above. In-memory reads take milliseconds, so with lazy
    // start this only costs test time and the message stays buffered.
    std::thread::sleep(Duration::from_secs(3));

    let mut receiver = listener.subscribe(&"test_repo".to_string());

    let second_bookmark_json = r#"
        {
            "int": {
                "time": 1744712410
            },
            "normal": {
                "repo_name": "test_repo",
                "bookmark_kind": "test_bookmark_kind",
                "bookmark_name": "second_bookmark",
                "old_bookmark_value": "test_old_bookmark_value",
                "new_bookmark_value": "test_new_bookmark_value",
                "update_reason": "test_update_reason",
                "operation": "test_operation",
                "tw_job_handle": "test_tw_job_handle",
                "client_entry_point": "test_client_entry_point"
            }
        }
    "#;
    producer.write_message(&mut Cursor::new(second_bookmark_json.as_bytes()), None)?;

    // The pre-subscribe message arrives first: it was never consumed early.
    let first = receiver.recv().await?;
    assert_eq!(first.bookmark_name, "first_bookmark".to_string());
    let second = receiver.recv().await?;
    assert_eq!(second.bookmark_name, "second_bookmark".to_string());
    Ok(())
}
