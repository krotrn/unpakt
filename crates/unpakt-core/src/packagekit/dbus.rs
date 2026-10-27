//! PackageKit over the system D-Bus.
//!
//! zbus is async, and each bus operation here runs to its end on the calling
//! thread under `zbus::block_on`, so the client stays blocking. Every method
//! call is bounded by `QUERY_TIMEOUT`. The connection handshake
//! has no timeout of its own and is bounded only in the capability probe,
//! which runs on its own thread. PackageKit's transaction methods return once
//! the transaction is queued; polkit prompts and progress arrive afterwards as
//! signals, and that wait is unbounded for an install or removal, which a
//! prompt may hold for as long as the user takes. A query (`Resolve`) waits at
//! most `QUERY_TIMEOUT` for each call and `QUERY_TIMEOUT` more for its signals.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_lite::StreamExt;
use zbus::connection::Builder;
use zbus::message::Type;
use zbus::{
    Connection, DynamicType, MatchRule, Message, MessageStream, ObjectPath, OwnedObjectPath,
    OwnedVariant,
};

use super::{PackageKit, PkEvent, PkPackage};
use crate::cancel::Cancel;
use crate::error::{Error, Result};

const SERVICE: &str = "org.freedesktop.PackageKit";
const SERVICE_PATH: &str = "/org/freedesktop/PackageKit";
const TRANSACTION_IFACE: &str = "org.freedesktop.PackageKit.Transaction";
const BUS_SERVICE: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const PROPERTIES_IFACE: &str = "org.freedesktop.DBus.Properties";

// The wire values of PackageKit 1.2.x to 1.4.x: each enum counts up from zero
// in the order of lib/pk-enum.h (PkRoleEnum, PkStatusEnum, PkExitEnum,
// PkFilterEnum, PkErrorEnum), and D-Bus sends them as plain integers.
const ROLE_INSTALL_FILES: u32 = 10;
const STATUS_UNKNOWN: u32 = 0;
const STATUS_WAIT: u32 = 1;
const STATUS_SETUP: u32 = 2;
const STATUS_FINISHED: u32 = 18;
const STATUS_WAITING_FOR_LOCK: u32 = 30;
const STATUS_WAITING_FOR_AUTH: u32 = 31;
const EXIT_SUCCESS: u32 = 1;
const EXIT_CANCELLED: u32 = 3;
const FILTER_INSTALLED: u32 = 2;
#[cfg(test)]
const ERROR_DEP_RESOLUTION_FAILED: u32 = 13;
const ERROR_TRANSACTION_CANCELLED: u32 = 17;
const ERROR_NOT_AUTHORIZED: u32 = 48;

/// How often a running transaction checks whether to cancel.
const POLL: Duration = Duration::from_millis(200);

/// How long a bus call, and a query's wait for its signals, may take before
/// PackageKit counts as not answering. A query is a few calls plus the wait,
/// so it can take several times this; the capability probe as a whole takes
/// this long at most.
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

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

/// Whether a transaction that began at `started` has used up `limit`.
fn timed_out(started: Instant, limit: Option<Duration>, now: Instant) -> bool {
    limit.is_some_and(|limit| now.saturating_duration_since(started) >= limit)
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
            let (iface, changed, _): (String, HashMap<String, OwnedVariant>, Vec<String>) =
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

/// The next message of `stream`, `None` when none arrives within `wait`.
/// `Some(None)` means the stream ended.
fn next_message(
    stream: &mut MessageStream,
    wait: Duration,
) -> Option<Option<zbus::Result<Message>>> {
    zbus::block_on(async {
        // Inside `block_on` this is the runtime the thread is driving, so its
        // timer runs here.
        match zruntime::SharedRuntime::current() {
            Ok(runtime) => runtime.timeout(wait, stream.next()).await.ok(),
            Err(e) => Some(Some(Err(e.into()))),
        }
    })
}

/// Calls `method` of `iface` at `path` on `service` and waits for the reply,
/// which the connection's method timeout bounds.
fn call<'p, P, R, B>(
    conn: &Connection,
    service: &str,
    path: P,
    iface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<R>
where
    P: TryInto<ObjectPath<'p>>,
    P::Error: Into<zbus::Error>,
    R: for<'d> zbus::wire::DynamicDeserialize<'d>,
    B: serde::Serialize + DynamicType,
{
    let reply = zbus::block_on(conn.call_method(Some(service), path, Some(iface), method, body))?;
    reply.body().deserialize()
}

/// Calls `method` on the transaction at `path`, which returns nothing.
fn tx_call<B>(conn: &Connection, path: &OwnedObjectPath, method: &str, body: &B) -> zbus::Result<()>
where
    B: serde::Serialize + DynamicType,
{
    call(
        conn,
        SERVICE,
        path.as_ref(),
        TRANSACTION_IFACE,
        method,
        body,
    )
}

/// Whether `e` is an error reply. zbus 6 reports a reply named in
/// `org.freedesktop.DBus.Error` as `FDO` and any other as `MethodError`.
fn is_error_reply(e: &zbus::Error) -> bool {
    matches!(e, zbus::Error::MethodError(..) | zbus::Error::FDO(_))
}

/// Whether a PackageKit method changes the installed packages.
fn changes_system(method: &str) -> bool {
    matches!(method, "InstallFiles" | "RemovePackages")
}

fn bus_error(e: zbus::Error) -> Error {
    call_error(e, false)
}

/// Maps a failed bus call. A timed-out call that changes the system may
/// still have been queued, which the message says.
fn call_error(e: zbus::Error, changes_system: bool) -> Error {
    match e {
        zbus::Error::InputOutput(ref io) if io.kind() == std::io::ErrorKind::TimedOut => {
            Error::PackageKit(
                if changes_system {
                    "PackageKit did not answer; the change may still be applied"
                } else {
                    "PackageKit did not answer"
                }
                .into(),
            )
        }
        e => Error::PackageKit(e.to_string()),
    }
}

/// PackageKit reached through the system bus. The connection opens on first
/// use and is kept.
pub struct Dbus {
    conn: Arc<Mutex<Option<Connection>>>,
}

/// The kept connection, opened if there is none yet. The lock is not held
/// while connecting, so a stuck handshake cannot block other callers.
fn open(slot: &Mutex<Option<Connection>>) -> Result<Connection> {
    if let Some(conn) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        return Ok(conn.clone());
    }
    let conn =
        zbus::block_on(Builder::system().method_timeout(QUERY_TIMEOUT).build()).map_err(|e| {
            Error::PackageKitUnavailable(format!("the system bus is not reachable: {e}"))
        })?;
    let mut kept = slot.lock().unwrap_or_else(|e| e.into_inner());
    Ok(kept.get_or_insert(conn).clone())
}

impl Dbus {
    pub fn new() -> Dbus {
        Dbus {
            conn: Arc::new(Mutex::new(None)),
        }
    }

    fn connection(&self) -> Result<Connection> {
        open(&self.conn)
    }

    fn probe(conn: Connection) -> zbus::Result<bool> {
        let present = call::<_, bool, _>(
            &conn,
            BUS_SERVICE,
            BUS_PATH,
            BUS_SERVICE,
            "NameHasOwner",
            &(SERVICE,),
        )? || call::<_, Vec<String>, _>(
            &conn,
            BUS_SERVICE,
            BUS_PATH,
            BUS_SERVICE,
            "ListActivatableNames",
            &(),
        )?
        .iter()
        .any(|n| n == SERVICE);
        if !present {
            return Ok(false);
        }
        let roles: OwnedVariant = call(
            &conn,
            SERVICE,
            SERVICE_PATH,
            PROPERTIES_IFACE,
            "Get",
            &(SERVICE, "Roles"),
        )?;
        let roles = u64::try_from(&roles)?;
        Ok(has_role(roles, ROLE_INSTALL_FILES))
    }

    /// Runs one transaction: creates it, listens, calls `method`, and follows
    /// it to its end. Returns the ids of the packages it reported. `limit`
    /// bounds how long the transaction may run; the signal subscription ends
    /// when this returns, whichever way.
    fn transact<B>(
        &self,
        method: &str,
        body: &B,
        limit: Option<Duration>,
        cancel: &Cancel,
        on: &mut dyn FnMut(PkEvent),
    ) -> Result<Vec<String>>
    where
        B: serde::Serialize + DynamicType,
    {
        let conn = self.connection()?;
        let path: OwnedObjectPath = call(
            &conn,
            SERVICE,
            SERVICE_PATH,
            SERVICE,
            "CreateTransaction",
            &(),
        )
        .map_err(|e| {
            if is_error_reply(&e) {
                Error::PackageKitUnavailable(e.to_string())
            } else {
                bus_error(e)
            }
        })?;
        tx_call(&conn, &path, "SetHints", &(vec!["interactive=true"],)).map_err(bus_error)?;

        // Listen before the call so that no signal is missed: the stream is
        // returned only once the bus has answered `AddMatch`.
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .path(path.as_ref())
            .build()
            .map_err(bus_error)?;
        let mut messages =
            zbus::block_on(MessageStream::for_match_rule(rule, &conn, None)).map_err(bus_error)?;

        tx_call(&conn, &path, method, body).map_err(|e| call_error(e, changes_system(method)))?;

        let (mut status, mut percentage) = (STATUS_UNKNOWN, 101);
        let mut last_event = None;
        let mut failure = None;
        let mut packages = Vec::new();
        let mut cancel_sent = false;
        let started = Instant::now();
        // Installs and removals have no overall deadline: the calls above
        // return once the transaction is queued, and a polkit prompt may then
        // wait on the user as long as it takes. Cancelling is the way out
        // until changes start.
        loop {
            if timed_out(started, limit, Instant::now()) {
                return Err(Error::PackageKit("PackageKit did not answer".into()));
            }
            let signals = match next_message(&mut messages, POLL) {
                Some(Some(Ok(msg))) => signal_of(&msg).unwrap_or_default(),
                Some(Some(Err(e))) => return Err(bus_error(e)),
                Some(None) => {
                    return Err(Error::PackageKit(
                        "lost the connection to PackageKit".into(),
                    ));
                }
                None => Vec::new(),
            };
            for signal in signals {
                match signal {
                    Signal::Package(id) => packages.push(id),
                    Signal::ErrorCode(code, details) => {
                        failure.get_or_insert_with(|| error_for(code, &details));
                    }
                    Signal::Finished(exit) => {
                        return match exit {
                            EXIT_CANCELLED => Err(Error::Interrupted),
                            _ => match failure {
                                Some(e) => Err(e),
                                None if exit == EXIT_SUCCESS => Ok(packages),
                                None => Err(Error::PackageKit("the transaction failed".into())),
                            },
                        };
                    }
                    Signal::Status(s) => status = s,
                    Signal::Percentage(p) => percentage = p,
                }
            }
            let event = event_for(status, percentage);
            if let Some(event) = event.filter(|e| Some(*e) != last_event) {
                on(event);
            }
            last_event = event;
            if !cancel_sent && before_changes(status) && cancel.is_cancelled() {
                cancel_sent = true;
                if let Err(e) = tx_call(&conn, &path, "Cancel", &()) {
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
        // The handshake has no timeout, so connecting happens on the probe's
        // thread too and the whole probe is bounded here.
        let (send, recv) = mpsc::channel();
        let slot = Arc::clone(&self.conn);
        std::thread::spawn(move || {
            let result = open(&slot)
                .map_err(|e| e.to_string())
                .and_then(|conn| Dbus::probe(conn).map_err(|e| e.to_string()));
            let _ = send.send(result);
        });
        match recv.recv_timeout(QUERY_TIMEOUT) {
            Ok(Ok(ready)) => ready,
            Ok(Err(e)) => {
                tracing::debug!("PackageKit is not usable: {e}");
                false
            }
            Err(_) => {
                tracing::debug!("PackageKit did not answer the capability probe");
                false
            }
        }
    }

    fn resolve_installed(&self, name: &str) -> Result<Option<PkPackage>> {
        let ids = self.transact(
            "Resolve",
            &(1u64 << FILTER_INSTALLED, vec![name]),
            Some(QUERY_TIMEOUT),
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
        self.transact("InstallFiles", &(0u64, vec![file]), None, cancel, on)
            .map(|_| ())
    }

    fn remove(&self, package_id: &str, cancel: &Cancel, on: &mut dyn FnMut(PkEvent)) -> Result<()> {
        self.transact(
            "RemovePackages",
            &(0u64, vec![package_id], false, false),
            None,
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
    fn deadline_applies_only_when_set() {
        let start = Instant::now();
        let limit = Some(Duration::from_secs(30));
        assert!(!timed_out(start, limit, start + Duration::from_secs(29)));
        assert!(timed_out(start, limit, start + Duration::from_secs(30)));
        assert!(!timed_out(start, None, start + Duration::from_secs(86_400)));
    }

    #[test]
    fn timed_out_call_is_not_an_answer() {
        let timeout = zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::from(
            std::io::ErrorKind::TimedOut,
        )));
        assert_eq!(
            bus_error(timeout).to_string(),
            "package manager error: PackageKit did not answer"
        );
        assert!(
            bus_error(zbus::Error::Failure("boom".into()))
                .to_string()
                .contains("boom")
        );
    }

    #[test]
    fn timed_out_change_may_still_apply() {
        let timeout = || {
            zbus::Error::InputOutput(std::sync::Arc::new(std::io::Error::from(
                std::io::ErrorKind::TimedOut,
            )))
        };
        assert_eq!(
            call_error(timeout(), true).to_string(),
            "package manager error: PackageKit did not answer; the change may still be applied"
        );
        assert_eq!(
            call_error(timeout(), false).to_string(),
            "package manager error: PackageKit did not answer"
        );
        assert!(changes_system("InstallFiles") && changes_system("RemovePackages"));
        assert!(!changes_system("Resolve") && !changes_system("Cancel"));
    }

    #[test]
    fn enum_values_are_the_wire_values() {
        assert_eq!(ROLE_INSTALL_FILES, 10);
        assert_eq!(STATUS_UNKNOWN, 0);
        assert_eq!(STATUS_WAIT, 1);
        assert_eq!(STATUS_SETUP, 2);
        assert_eq!(STATUS_FINISHED, 18);
        assert_eq!(STATUS_WAITING_FOR_LOCK, 30);
        assert_eq!(STATUS_WAITING_FOR_AUTH, 31);
        assert_eq!(EXIT_SUCCESS, 1);
        assert_eq!(EXIT_CANCELLED, 3);
        assert_eq!(FILTER_INSTALLED, 2);
        assert_eq!(ERROR_DEP_RESOLUTION_FAILED, 13);
        assert_eq!(ERROR_TRANSACTION_CANCELLED, 17);
        assert_eq!(ERROR_NOT_AUTHORIZED, 48);
    }

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
