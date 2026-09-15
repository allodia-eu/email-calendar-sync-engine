//! iCalendar organiser and attendee content lines.

use engine_provider::{Invitee, InviteeRole, SchedulingIdentity};

use crate::unfold::{split_once_unquoted, split_unquoted};

pub(crate) fn organiser_line(identity: &SchedulingIdentity) -> String {
    identity_line("ORGANIZER", identity, None)
}

pub(crate) fn attendee_line(invitee: &Invitee) -> String {
    identity_line("ATTENDEE", invitee.identity(), Some(invitee.role()))
}

pub(crate) fn attendee_address(line: &str) -> Option<&str> {
    let (_, value) = split_once_unquoted(line, ':')?;
    Some(
        value
            .trim()
            .strip_prefix("mailto:")
            .or_else(|| value.trim().strip_prefix("MAILTO:"))
            .unwrap_or(value.trim()),
    )
}

pub(crate) fn update_attendee_line(line: &str, invitee: &Invitee) -> String {
    let (head, _) = split_once_unquoted(line, ':').unwrap_or(("ATTENDEE", ""));
    let mut parameters: Vec<String> = split_unquoted(head, ';')
        .into_iter()
        .skip(1)
        .filter(|parameter| {
            let name = parameter
                .split_once('=')
                .map_or(*parameter, |(name, _)| name);
            !(name.eq_ignore_ascii_case("ROLE")
                || invitee.identity().name().is_some() && name.eq_ignore_ascii_case("CN"))
        })
        .map(str::to_owned)
        .collect();
    if let Some(name) = invitee.identity().name() {
        parameters.push(format!("CN={}", escape_parameter(name)));
    }
    parameters.push(format!("ROLE={}", role_value(invitee.role())));

    let mut updated = "ATTENDEE".to_owned();
    for parameter in parameters {
        updated.push(';');
        updated.push_str(&parameter);
    }
    updated.push_str(":mailto:");
    updated.push_str(invitee.identity().address().as_str());
    updated
}

fn identity_line(
    property: &str,
    identity: &SchedulingIdentity,
    role: Option<InviteeRole>,
) -> String {
    let mut line = property.to_owned();
    if let Some(name) = identity.name() {
        line.push_str(";CN=");
        line.push_str(&escape_parameter(name));
    }
    if let Some(role) = role {
        line.push_str(";CUTYPE=INDIVIDUAL;ROLE=");
        line.push_str(role_value(role));
        line.push_str(";PARTSTAT=NEEDS-ACTION;RSVP=TRUE");
    }
    line.push_str(":mailto:");
    line.push_str(identity.address().as_str());
    line
}

const fn role_value(role: InviteeRole) -> &'static str {
    match role {
        InviteeRole::Required => "REQ-PARTICIPANT",
        InviteeRole::Optional => "OPT-PARTICIPANT",
    }
}

fn escape_parameter(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '^' => escaped.push_str("^^"),
            '\n' => escaped.push_str("^n"),
            '"' => escaped.push_str("^'"),
            '\r' => {}
            other if other.is_control() => {}
            other => escaped.push(other),
        }
    }
    if escaped.contains([':', ';', ',']) {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

#[cfg(test)]
mod tests {
    use engine_provider::{CalendarAddress, Invitee, SchedulingIdentity};

    use super::*;

    #[test]
    fn rfc_6868_escapes_parameter_text() {
        let invitee = Invitee::required(SchedulingIdentity::named(
            CalendarAddress::parse("guest@example.com").unwrap(),
            "Caret ^, quote \" and\nline",
        ));
        assert!(
            attendee_line(&invitee)
                .contains("CN=\"Caret ^^, quote ^' and^nline\";CUTYPE=INDIVIDUAL")
        );
    }
}
