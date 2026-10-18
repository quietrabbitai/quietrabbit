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
