// items.id=630 stage C: in a non-test build of the lib (which is what an
// integration test links), deriving a store key for a user with no registered
// key epoch is an ERROR, not a silent fall back to the initial epoch. A
// store opened without a login would otherwise key a rotated account's
// database with the wrong epoch's key.

use quietrabbit_lib::auth::epoch_guard;
use quietrabbit_lib::auth::subkeys::{self, DbPurpose, SubkeyError};

const DEK_HEX: &str = "4242424242424242424242424242424242424242424242424242424242424242";

#[test]
fn deriving_without_a_registered_epoch_is_an_error() {
    let user = "resident-epoch-test-unregistered";
    assert_eq!(epoch_guard::resident_epoch(user), None);
    assert_eq!(
        subkeys::resident_or_initial_epoch(user),
        Err(SubkeyError::NoResidentEpoch(user.to_owned()))
    );
    assert!(matches!(
        subkeys::db_key_hex(user, DEK_HEX, DbPurpose::Personal, "persona-1"),
        Err(SubkeyError::NoResidentEpoch(_))
    ));
}

#[test]
fn deriving_with_a_registered_epoch_uses_that_epoch() {
    let user = "resident-epoch-test-registered";
    epoch_guard::set_resident(user, 3);
    let derived = subkeys::db_key_hex(user, DEK_HEX, DbPurpose::Personal, "persona-1").unwrap();
    let explicit =
        subkeys::derive_db_key_hex(DEK_HEX, DbPurpose::Personal, "persona-1", user, 3).unwrap();
    let other_epoch =
        subkeys::derive_db_key_hex(DEK_HEX, DbPurpose::Personal, "persona-1", user, 1).unwrap();
    epoch_guard::clear_resident_for(user);
    assert_eq!(*derived, *explicit);
    assert_ne!(*derived, *other_epoch);
    // Cleared again: back to the error.
    assert!(subkeys::resident_or_initial_epoch(user).is_err());
}

// items.id=781: a reply or run that started in a session must still derive its
// store keys after that session ended (sign-out, idle lock). The key registry's
// clear forgets the resident epoch; a live writer lease pins the epoch the work
// started under, so the work saves under its own key and not under a guess.
#[test]
fn a_live_writer_lease_keeps_the_epoch_after_the_session_clears() {
    use quietrabbit_lib::auth::writer_gate::{self, Kind};

    let user = "resident-epoch-test-lease-pin";
    epoch_guard::set_resident(user, 3);
    let lease = writer_gate::acquire(user, Kind::Reply).unwrap();

    // The session ends: the registry's clear forgets the resident epoch.
    epoch_guard::clear_resident_for(user);
    assert_eq!(epoch_guard::resident_epoch(user), None);

    assert_eq!(subkeys::resident_or_initial_epoch(user), Ok(3));
    let derived = subkeys::db_key_hex(user, DEK_HEX, DbPurpose::Messages, "persona-1").unwrap();
    let explicit =
        subkeys::derive_db_key_hex(DEK_HEX, DbPurpose::Messages, "persona-1", user, 3).unwrap();
    assert_eq!(*derived, *explicit);

    // The pin ends with the lease: no work is left to finish, so no guess.
    drop(lease);
    assert_eq!(
        subkeys::resident_or_initial_epoch(user),
        Err(SubkeyError::NoResidentEpoch(user.to_owned()))
    );
}

#[test]
fn a_lease_pin_is_per_user() {
    use quietrabbit_lib::auth::writer_gate::{self, Kind};

    let (a, b) = ("resident-epoch-test-pin-a", "resident-epoch-test-pin-b");
    epoch_guard::set_resident(a, 5);
    let lease = writer_gate::acquire(a, Kind::FocusRun).unwrap();
    epoch_guard::clear_resident_for(a);

    assert_eq!(subkeys::resident_or_initial_epoch(a), Ok(5));
    assert_eq!(
        subkeys::resident_or_initial_epoch(b),
        Err(SubkeyError::NoResidentEpoch(b.to_owned()))
    );
    drop(lease);
}

#[test]
fn a_resident_epoch_wins_over_a_lease_pin() {
    use quietrabbit_lib::auth::writer_gate::{self, Kind};

    // Signed out and back in under a newer epoch while work from the older
    // session is still running: the live session's epoch is the current one.
    let user = "resident-epoch-test-resident-wins";
    epoch_guard::set_resident(user, 2);
    let lease = writer_gate::acquire(user, Kind::Reply).unwrap();
    epoch_guard::set_resident(user, 4);
    assert_eq!(subkeys::resident_or_initial_epoch(user), Ok(4));
    epoch_guard::clear_resident_for(user);
    drop(lease);
}
