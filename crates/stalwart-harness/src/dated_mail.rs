//! Mail carrying a delivery date the test chooses, planted over JMAP.
//!
//! A sync-depth test needs mail older than the window it syncs under, and the seed holds none:
//! an IMAP `APPEND` without a date is stamped with the moment of seeding. JMAP `Email/set` takes
//! `receivedAt` on create (RFC 8621 §4.6), and Stalwart reports the same instant as the IMAP
//! `INTERNALDATE`, so one plant serves both protocols' suites.

use serde_json::{Value, json};

use crate::{Harness, HarnessError};

/// The capabilities every call here uses.
const USING: [&str; 2] = ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

impl Harness {
    /// Files a message with `subject`, dated `received_at` (RFC 3339, UTC), into the mailbox
    /// holding `role` (`"inbox"`, `"sent"`, …) in `auth`'s account, and returns its JMAP id.
    ///
    /// # Errors
    /// Returns [`HarnessError::Protocol`] when the account has no mailbox with that role or
    /// the server refuses the message.
    pub fn plant_dated_email_as(
        &self,
        auth: (&str, &str),
        role: &str,
        subject: &str,
        received_at: &str,
    ) -> Result<String, HarnessError> {
        let account = self.mail_account_as(auth)?;
        let found = self.call_as(
            auth,
            "Mailbox/query",
            &json!({"accountId": account, "filter": {"role": role}}),
        )?;
        let mailbox = found["ids"][0].as_str().ok_or_else(|| {
            HarnessError::protocol("jmap", format!("no mailbox with role {role}: {found}"))
        })?;
        let email = json!({
            "mailboxIds": {mailbox: true},
            "keywords": {"$seen": true},
            "receivedAt": received_at,
            "sentAt": received_at,
            "subject": subject,
            "from": [{"email": auth.0}],
            "to": [{"email": "planted@example.test"}],
            "bodyValues": {"body": {"value": subject}},
            "textBody": [{"partId": "body", "type": "text/plain"}],
        });
        let set = self.call_as(
            auth,
            "Email/set",
            &json!({"accountId": account, "create": {"planted": email}}),
        )?;
        set["created"]["planted"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| HarnessError::protocol("jmap", format!("not created: {set}")))
    }

    /// The ids of every email in `auth`'s account whose subject contains `text`: what an
    /// interrupted run left behind, to destroy before planting again.
    ///
    /// # Errors
    /// Returns [`HarnessError::Protocol`] on a malformed response.
    pub fn emails_with_subject_as(
        &self,
        auth: (&str, &str),
        text: &str,
    ) -> Result<Vec<String>, HarnessError> {
        let account = self.mail_account_as(auth)?;
        let found = self.call_as(
            auth,
            "Email/query",
            &json!({"accountId": account, "filter": {"subject": text}}),
        )?;
        let ids = found["ids"]
            .as_array()
            .ok_or_else(|| HarnessError::protocol("jmap", format!("no ids: {found}")))?;
        Ok(ids
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect())
    }

    /// Destroys the emails named by `ids` in `auth`'s account.
    ///
    /// # Errors
    /// Returns [`HarnessError::Protocol`] when the server refuses a destroy.
    pub fn destroy_emails_as(
        &self,
        auth: (&str, &str),
        ids: &[String],
    ) -> Result<(), HarnessError> {
        if ids.is_empty() {
            return Ok(());
        }
        let account = self.mail_account_as(auth)?;
        let set = self.call_as(
            auth,
            "Email/set",
            &json!({"accountId": account, "destroy": ids}),
        )?;
        match set.get("notDestroyed") {
            Some(refused) if !refused.is_null() => Err(HarnessError::protocol(
                "jmap",
                format!("not destroyed: {refused}"),
            )),
            _ => Ok(()),
        }
    }

    /// `auth`'s primary mail account id.
    fn mail_account_as(&self, auth: (&str, &str)) -> Result<String, HarnessError> {
        let session = self.jmap_session_as(auth)?;
        session["primaryAccounts"]["urn:ietf:params:jmap:mail"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| HarnessError::protocol("jmap", "no primary mail account"))
    }

    /// One method call as `auth`, answered by its arguments object.
    fn call_as(
        &self,
        auth: (&str, &str),
        method: &str,
        arguments: &Value,
    ) -> Result<Value, HarnessError> {
        let body = json!({"using": USING, "methodCalls": [[method, arguments, "c"]]});
        let response = self.jmap_post_as(auth, body.to_string().as_bytes())?;
        if response.status != 200 {
            return Err(HarnessError::protocol(
                "jmap",
                format!("{method} returned HTTP {}", response.status),
            ));
        }
        let parsed: Value = serde_json::from_slice(&response.body)?;
        let answer = &parsed["methodResponses"][0];
        if answer[0] != method {
            return Err(HarnessError::protocol(
                "jmap",
                format!("{method} answered {answer}"),
            ));
        }
        Ok(answer[1].clone())
    }
}
