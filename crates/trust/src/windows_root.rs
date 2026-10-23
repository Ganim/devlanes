//! Wording shared by the two stores that change the Windows user root store:
//! `devlanes-trust-windows` through the certificate store API and, from WSL,
//! `devlanes-trust-linux` through `certutil.exe`.

use crate::TrustError;

/// What status lines call the real `Root` store.
pub const LOCATION: &str = "Windows user root store";

/// The confirmation every `Root` change needs, short enough for `doctor`'s fix line.
pub const APPROVAL: &str = "choose Yes in the Windows security warning";

/// A change to the user root store, which Windows confirms in a dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootChange {
    /// Adding the CA.
    Add,
    /// Deleting the CA.
    Delete,
}

impl RootChange {
    /// The devlanes command that makes this change, for the user to rerun.
    pub fn command(self) -> &'static str {
        match self {
            Self::Add => "devlanes certs trust",
            Self::Delete => "devlanes certs untrust",
        }
    }

    /// The quoted title of the dialog Windows shows for it.
    pub fn dialog(self) -> &'static str {
        match self {
            Self::Add => "\"Security Warning\"",
            Self::Delete => "\"Root Certificate Store\"",
        }
    }

    /// What the change does to the store at `path` (`Cert:\CurrentUser\Root`), for messages.
    pub fn action(self, path: &str) -> String {
        match self {
            Self::Add => format!("adding the CA to {path}"),
            Self::Delete => format!("removing the CA from {path}"),
        }
    }

    /// [`TrustError::Cancelled`] for a No in the dialog of this change to `path`.
    pub fn declined(self, path: &str) -> TrustError {
        TrustError::Cancelled {
            detail: format!(
                "{} was declined in the Windows {} dialog; run `{}` again and choose Yes",
                self.action(path),
                self.dialog(),
                self.command()
            ),
        }
    }

    /// The notice shown right before the change: the dialog it is about to show.
    /// `count` is how many CAs a delete removes, one dialog each.
    pub fn pending(self, count: usize) -> String {
        let what = match self {
            Self::Add => "adding the CA to".to_owned(),
            Self::Delete if count > 1 => format!("each of the {count} CAs devlanes removes from"),
            Self::Delete => "removing the CA from".to_owned(),
        };
        format!(
            "Windows asks you to confirm {what} your user root store in a {} dialog; choose Yes",
            self.dialog()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A No names the dialog of that change and the command to rerun.
    #[test]
    fn declined_dialogs_name_their_dialog_and_command() {
        let detail = |change: RootChange| match change.declined(r"Cert:\CurrentUser\Root") {
            TrustError::Cancelled { detail } => detail,
            other => panic!("{other:?}"),
        };
        let add = detail(RootChange::Add);
        assert!(add.contains("\"Security Warning\""), "{add}");
        assert!(add.contains("`devlanes certs trust`"), "{add}");
        let delete = detail(RootChange::Delete);
        assert!(delete.contains("\"Root Certificate Store\""), "{delete}");
        assert!(delete.contains("`devlanes certs untrust`"), "{delete}");
    }

    #[test]
    fn pending_notices_name_the_dialog() {
        assert_eq!(
            RootChange::Add.pending(1),
            "Windows asks you to confirm adding the CA to your user root store in a \
             \"Security Warning\" dialog; choose Yes"
        );
        assert!(
            RootChange::Delete
                .pending(2)
                .contains("each of the 2 CAs devlanes removes from your user root store")
        );
        assert!(
            RootChange::Delete.pending(1).contains(
                "removing the CA from your user root store in a \"Root Certificate Store\""
            )
        );
    }
}
