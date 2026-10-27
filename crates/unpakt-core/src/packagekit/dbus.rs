//! PackageKit over the system D-Bus.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::message::Type;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{MatchRule, Message};

use super::{PackageKit, PkEvent, PkPackage};
use crate::cancel::Cancel;
use crate::error::{Error, Result};

const SERVICE: &str = "org.freedesktop.PackageKit";
const SERVICE_PATH: &str = "/org/freedesktop/PackageKit";
const TRANSACTION_IFACE: &str = "org.freedesktop.PackageKit.Transaction";

// Enum values count up from zero in the order of PackageKit's
// lib/pk-enum.h (PkRoleEnum, PkStatusEnum, PkExitEnum, PkFilterEnum,
// PkErrorEnum); D-Bus sends them as plain integers.
const ROLE_INSTALL_FILES: u32 = 10;
const STATUS_UNKNOWN: u32 = 0;
const STATUS_WAIT: u32 = 1;
const STATUS_SETUP: u32 = 2;
const STATUS_FINISHED: u32 = 18;
const STATUS_WAITING_FOR_LOCK: u32 = 27;
const STATUS_WAITING_FOR_AUTH: u32 = 28;
const EXIT_SUCCESS: u32 = 1;
const EXIT_CANCELLED: u32 = 3;
const FILTER_INSTALLED: u32 = 2;
#[cfg(test)]
const ERROR_DEP_RESOLUTION_FAILED: u32 = 13;
const ERROR_TRANSACTION_CANCELLED: u32 = 17;
const ERROR_NOT_AUTHORIZED: u32 = 46;

/// How often a running transaction checks whether to cancel.
const POLL: Duration = Duration::from_millis(200);

/// Whether the `Roles` bitfield of PackageKit includes `role`.
fn has_role(roles: u64, role: u32) -> bool {
    roles & (1 << role) != 0
}

/// Whether a transaction has not yet started changing the system, so that
/// cancelling it still leaves everything as it was.
fn before_changes(status: u32) -> bool {
    matches!(
        status,
        STATUS_UNKNOWN
            | STATUS_WAIT
            | STATUS_SETUP
            | STATUS_WAITING_FOR_AUTH
            | STATUS_WAITING_FOR_LOCK
    )
}

/// The event a status and percentage stand for, if any.
fn event_for(status: u32, percentage: u32) -> Option<PkEvent> {
    if status == STATUS_WAITING_FOR_AUTH {
        return Some(PkEvent::WaitingForAuth);
    }
    if status == STATUS_FINISHED || before_changes(status) {
        return None;
    }
    // PackageKit sends 101 when the percentage is unknown.
    let percent = match percentage {
        0..=100 => Some(percentage as u8),
        _ => None,
    };
    Some(PkEvent::Changing { percent })
}

/// The error a transaction's `ErrorCode` signal stands for.
fn error_for(code: u32, details: &str) -> Error {
    match code {
        ERROR_NOT_AUTHORIZED => {
            Error::Refused("authorization was refused; nothing was changed".into())
        }
        ERROR_TRANSACTION_CANCELLED => Error::Interrupted,
        _ => Error::PackageKit(details.to_string()),
    }
}

/// What a transaction tells its caller.
enum Signal {
    Package(String),
    ErrorCode(u32, String),
    Finished(u32),
    Status(u32),
    Percentage(u32),
}

/// Reads a transaction signal; `None` for messages that are not one.
fn signal_of(msg: &Message) -> Option<Vec<Signal>> {
    let header = msg.header();
    let member = header.member()?.as_str();
    let body = msg.body();
    match member {
        "Package" => {
            let (_info, id, _summary): (u32, String, String) = body.deserialize().ok()?;
            Some(vec![Signal::Package(id)])
        }
        "ErrorCode" => {
            let (code, details): (u32, String) = body.deserialize().ok()?;
            Some(vec![Signal::ErrorCode(code, details)])
        }
        "Finished" => {
            let (exit, _runtime): (u32, u32) = body.deserialize().ok()?;
            Some(vec![Signal::Finished(exit)])
        }
        "PropertiesChanged" => {
            let (iface, changed, _): (String, HashMap<String, OwnedValue>, Vec<String>) =
                body.deserialize().ok()?;
            if iface != TRANSACTION_IFACE {
                return None;
            }
            let mut out = Vec::new();
            if let Some(status) = changed.get("Status").and_then(|v| u32::try_from(v).ok()) {
                out.push(Signal::Status(status));
            }
            if let Some(p) = changed
                .get("Percentage")
                .and_then(|v| u32::try_from(v).ok())
            {
                out.push(Signal::Percentage(p));
            }
            Some(out)
        }
        _ => None,
    }
}

fn bus_error(e: zbus::Error) -> Error {
    Error::PackageKit(e.to_string())
}

/// PackageKit reached through the system bus. The connection opens on first
/// use and is kept.
pub struct Dbus {
    conn: Mutex<Option<Connection>>,
}

impl Dbus {
    pub fn new() -> Dbus {
        Dbus {
            conn: Mutex::new(None),
        }
    }

    fn connection(&self) -> Result<Connection> {
        let mut slot = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conn) = slot.as_ref() {
            return Ok(conn.clone());
        }
        let conn = Connection::system().map_err(|e| {
            Error::PackageKitUnavailable(format!("the system bus is not reachable: {e}"))
        })?;
        *slot = Some(conn.clone());
        Ok(conn)
    }

    fn probe(&self) -> zbus::Result<bool> {
        let conn = self
            .connection()
            .map_err(|e| zbus::Error::Failure(e.to_string()))?;
        let bus = DBusProxy::new(&conn)?;
        let name = zbus::names::BusName::try_from(SERVICE)?;
        let present = bus.name_has_owner(name)?
            || bus
                .list_activatable_names()?
                .iter()
                .any(|n| n.as_str() == SERVICE);
        if !present {
            return Ok(false);
        }
        let main = Proxy::new(&conn, SERVICE, SERVICE_PATH, SERVICE)?;
        let roles: u64 = main.get_property("Roles")?;
        Ok(has_role(roles, ROLE_INSTALL_FILES))
    }

    /// Runs one transaction: creates it, listens, calls `method`, and follows
    /// it to its end. Returns the ids of the packages it reported.
    fn transact<B>(
        &self,
        method: &str,
        body: &B,
        cancel: &Cancel,
        on: &mut dyn FnMut(PkEvent),
    ) -> Result<Vec<String>>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        let conn = self.connection()?;
        let main = Proxy::new(&conn, SERVICE, SERVICE_PATH, SERVICE).map_err(bus_error)?;
        let path: OwnedObjectPath = main.call("CreateTransaction", &()).map_err(|e| match e {
            zbus::Error::MethodError(..) => Error::PackageKitUnavailable(e.to_string()),
            other => bus_error(other),
        })?;
        let tx = Proxy::new(&conn, SERVICE, path.as_ref(), TRANSACTION_IFACE).map_err(bus_error)?;
        tx.call::<_, _, ()>("SetHints", &(vec!["interactive=true"],))
            .map_err(bus_error)?;

        // Listen before the call so that no signal is missed.
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .path(path.as_ref())
            .map_err(bus_error)?
            .build();
        let messages = MessageIterator::for_match_rule(rule, &conn, None).map_err(bus_error)?;
        let (send, recv) = mpsc::channel();
        std::thread::spawn(move || {
            for msg in messages.flatten() {
                let Some(signals) = signal_of(&msg) else {
                    continue;
                };
                for signal in signals {
                    let last = matches!(signal, Signal::Finished(_));
                    if send.send(signal).is_err() || last {
                        return;
                    }
                }
            }
        });

        tx.call::<_, _, ()>(method, body).map_err(bus_error)?;

        let (mut status, mut percentage) = (STATUS_UNKNOWN, 101);
        let mut last_event = None;
        let mut failure = None;
        let mut packages = Vec::new();
        let mut cancel_sent = false;
        loop {
            match recv.recv_timeout(POLL) {
                Ok(Signal::Package(id)) => packages.push(id),
                Ok(Signal::ErrorCode(code, details)) => {
                    failure.get_or_insert_with(|| error_for(code, &details));
                }
                Ok(Signal::Finished(exit)) => {
                    return match exit {
                        EXIT_CANCELLED => Err(Error::Interrupted),
                        _ => match failure {
                            Some(e) => Err(e),
                            None if exit == EXIT_SUCCESS => Ok(packages),
                            None => Err(Error::PackageKit("the transaction failed".into())),
                        },
                    };
                }
                Ok(Signal::Status(s)) => status = s,
                Ok(Signal::Percentage(p)) => percentage = p,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::PackageKit(
                        "lost the connection to PackageKit".into(),
                    ));
                }
            }
            let event = event_for(status, percentage);
            if let Some(event) = event.filter(|e| Some(*e) != last_event) {
                on(event);
            }
            last_event = event;
            if !cancel_sent && before_changes(status) && cancel.is_cancelled() {
                cancel_sent = true;
                if let Err(e) = tx.call::<_, _, ()>("Cancel", &()) {
                    tracing::debug!("cancelling the PackageKit transaction failed: {e}");
                }
            }
        }
    }
}

impl Default for Dbus {
    fn default() -> Self {
        Dbus::new()
    }
}

impl PackageKit for Dbus {
    fn can_install_files(&self) -> bool {
        self.probe().unwrap_or_else(|e| {
            tracing::debug!("PackageKit is not usable: {e}");
            false
        })
    }

    fn resolve_installed(&self, name: &str) -> Result<Option<PkPackage>> {
        let ids = self.transact(
            "Resolve",
            &(1u64 << FILTER_INSTALLED, vec![name]),
            &Cancel::new(),
            &mut |_| {},
        )?;
        Ok(ids
            .iter()
            .filter_map(|id| PkPackage::parse(id))
            .find(|p| p.name == name))
    }

    fn install_file(
        &self,
        file: &Path,
        cancel: &Cancel,
        on: &mut dyn FnMut(PkEvent),
    ) -> Result<()> {
        let file = file.to_string_lossy().into_owned();
        self.transact("InstallFiles", &(0u64, vec![file]), cancel, on)
            .map(|_| ())
    }

    fn remove(&self, package_id: &str, cancel: &Cancel, on: &mut dyn FnMut(PkEvent)) -> Result<()> {
        self.transact(
            "RemovePackages",
            &(0u64, vec![package_id], false, false),
            cancel,
            on,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNNING: u32 = STATUS_SETUP + 1;

    #[test]
    fn roles_bitfield() {
        let roles = 1u64 << ROLE_INSTALL_FILES;
        assert!(has_role(roles, ROLE_INSTALL_FILES));
        assert!(!has_role(roles, ROLE_INSTALL_FILES + 1));
        assert!(!has_role(0, ROLE_INSTALL_FILES));
    }

    #[test]
    fn waiting_for_auth_is_before_changes() {
        for s in [
            STATUS_UNKNOWN,
            STATUS_WAIT,
            STATUS_SETUP,
            STATUS_WAITING_FOR_AUTH,
            STATUS_WAITING_FOR_LOCK,
        ] {
            assert!(before_changes(s), "{s}");
        }
    }

    #[test]
    fn running_is_not_before_changes() {
        assert!(!before_changes(RUNNING));
        assert!(!before_changes(STATUS_FINISHED));
    }

    #[test]
    fn unknown_percentage_is_none() {
        assert_eq!(
            event_for(RUNNING, 101),
            Some(PkEvent::Changing { percent: None })
        );
        assert_eq!(
            event_for(RUNNING, 40),
            Some(PkEvent::Changing { percent: Some(40) })
        );
    }

    #[test]
    fn auth_wait_event() {
        assert_eq!(
            event_for(STATUS_WAITING_FOR_AUTH, 101),
            Some(PkEvent::WaitingForAuth)
        );
        assert_eq!(event_for(STATUS_SETUP, 0), None);
        assert_eq!(event_for(STATUS_FINISHED, 100), None);
    }

    #[test]
    fn not_authorized_is_a_refusal() {
        assert!(matches!(
            error_for(ERROR_NOT_AUTHORIZED, "no"),
            Error::Refused(m) if m == "authorization was refused; nothing was changed"
        ));
    }

    #[test]
    fn cancelled_is_interrupted() {
        assert!(matches!(
            error_for(ERROR_TRANSACTION_CANCELLED, ""),
            Error::Interrupted
        ));
    }

    #[test]
    fn other_errors_carry_details() {
        assert_eq!(
            error_for(ERROR_DEP_RESOLUTION_FAILED, "foo is needed by bar").to_string(),
            "package manager error: foo is needed by bar"
        );
    }
}
