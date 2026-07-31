//! Proves the real OS accepts our bindings and hands us exclusive ownership.
//!
//! Ignored by default: it needs a desktop session (and on Linux, X11), and its
//! results depend on what else is installed on the machine. Run with:
//!
//!   cargo test --test hotkey_registration -- --ignored --test-threads=1 --nocapture

use global_hotkey::GlobalHotKeyManager;
use spotify_control::config::Bindings;
use spotify_control::hotkeys::{default_bindings, describe_bindings, HotkeyRegistry};
use std::collections::HashMap;

#[test]
#[ignore = "requires a desktop session"]
fn the_os_grants_the_bindings_it_can() {
    let cfg = Bindings::default();
    let registry =
        HotkeyRegistry::register_all(&cfg).expect("no binding could be registered at all");
    let active = registry.active_actions();

    let keys: HashMap<_, _> = describe_bindings(&cfg).into_iter().collect();
    for action in &active {
        println!("  granted   {} -> {}", keys[action], action.label());
    }
    for conflict in registry.conflicts() {
        println!(
            "  REFUSED   {} -> {} ({})",
            conflict.keys,
            conflict.action.label(),
            conflict.reason
        );
    }

    // Whatever we did get must dispatch to the right action.
    for (action, hotkey) in default_bindings() {
        let expected = active.contains(&action).then_some(action);
        assert_eq!(
            registry.action_for(hotkey.id()),
            expected,
            "{action:?} did not round-trip through its hotkey id"
        );
    }

    assert_eq!(
        active.len() + registry.conflicts().len(),
        default_bindings().len(),
        "every binding must be accounted for as granted or refused"
    );
}

#[test]
#[ignore = "requires a desktop session"]
fn granted_bindings_are_exclusive_while_held() {
    let registry = HotkeyRegistry::register_all(&Bindings::default())
        .expect("no binding could be registered at all");
    let active = registry.active_actions();
    let held: Vec<_> = default_bindings()
        .into_iter()
        .filter(|(a, _)| active.contains(a))
        .collect();

    // A second manager standing in for another application must be refused,
    // which is what "other apps can't use these keys" actually means.
    let rival = GlobalHotKeyManager::new().expect("could not create a second manager");
    for (action, hotkey) in &held {
        assert!(
            rival.register(*hotkey).is_err(),
            "{action:?} was handed to a second app while we held it"
        );
    }

    // ...and releasing them puts the keys back for everyone else.
    drop(registry);
    for (action, hotkey) in &held {
        rival
            .register(*hotkey)
            .unwrap_or_else(|e| panic!("{action:?} stayed locked after release: {e}"));
        let _ = rival.unregister(*hotkey);
    }
}
