/// Mirrors IGSH-902: a new incoming message from someone else resurfaces a locally
/// archived chat back to the inbox, while own messages and reactions do not.
pub fn should_resurface_on_event(
    archived_local: bool,
    from_me: bool,
    is_reaction_only: bool,
) -> bool {
    archived_local && !from_me && !is_reaction_only
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_message_from_other_resurfaces_archived_chat() {
        assert!(should_resurface_on_event(true, false, false));
        assert!(!should_resurface_on_event(true, true, false));
        assert!(!should_resurface_on_event(true, false, true));
        assert!(!should_resurface_on_event(false, false, false));
    }
}
