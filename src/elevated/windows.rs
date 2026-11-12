//! Windows: the core creates the wintun adapter and sets its addresses, routes and DNS (gone with the adapter when the
//! core stops). Here: binding the core to the uplink, and WFP filters against DNS leaks and for the kill switch, in a
//! dynamic session that Windows removes when this process ends, however it ends. The core runs in this process's job.
use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr, TcpStream};
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Receiver;

use serde_json::{Value, json};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::NetworkManagement::IpHelper::{
    ConvertInterfaceAliasToLuid, ConvertInterfaceLuidToAlias, FreeMibTable, GetIpForwardTable2, GetIpInterfaceEntry, InitializeIpInterfaceEntry, MIB_IPFORWARD_ROW2,
    MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW,
};
use windows::Win32::NetworkManagement::Ndis::NET_LUID_LH;
use windows::Win32::NetworkManagement::WindowsFilteringPlatform::*;
use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
use windows::Win32::Networking::WinSock::AF_INET;
use windows::Win32::System::Rpc::RPC_C_AUTHN_WINNT;
use windows::core::{GUID, PCWSTR, PWSTR};

use super::Running;
use crate::xray::{TUN_ADDRS, TUN_NAME};

pub struct Peer;

/// `<port> <token>`: connect back to the app and prove it was the app that started us.
pub fn connect(args: &[String]) -> Result<(Box<dyn Write + Send>, Receiver<String>, Peer), String> {
    let [port, token] = args else { return Err("usage: --tun-helper <port> <token>".into()) };
    let port: u16 = port.parse().map_err(|_| format!("not a port: {port}"))?;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("cannot reach the app: {e}"))?;
    writeln!(stream, "hello {token}").map_err(|e| e.to_string())?;
    let lines = super::lines(stream.try_clone().map_err(|e| e.to_string())?);
    Ok((Box::new(stream), lines, Peer))
}

/// Nothing outlives this process: filters are dynamic, the adapter goes with the core, the core with our job.
pub fn cleanup(_: &Peer) {}

pub struct Net {
    wfp: Wfp,
    kill: Vec<u64>,
    uplink: Option<String>,
}

impl Net {
    /// The core sets the adapter's resolver itself.
    pub fn up(_: &Peer, xray: &Path, kill: bool, _: &str) -> Result<Net, String> {
        if !xray.with_file_name("wintun.dll").is_file() {
            return Err("wintun.dll is missing next to xray.exe".into());
        }
        let wfp = Wfp::open()?;
        wfp.base(xray)?;
        let mut net = Net { wfp, kill: vec![], uplink: uplink() };
        net.set_kill_switch(kill)?;
        Ok(net)
    }

    pub fn set_kill_switch(&mut self, on: bool) -> Result<(), String> {
        if on && self.kill.is_empty() {
            self.wfp.kill_switch(&mut self.kill)?;
        } else if !on {
            for id in self.kill.drain(..) {
                self.wfp.delete(id);
            }
        }
        Ok(())
    }

    pub fn configure(&self, settings: &mut Value) {
        if let Some(uplink) = &self.uplink {
            settings["autoOutboundsInterface"] = json!(uplink);
        }
    }

    pub fn command(&self, xray: &Path) -> Command {
        Command::new(xray)
    }

    pub fn spawn(&self, cmd: &mut Command) -> std::io::Result<Running> {
        // A restarted core recreates the adapter under the same name and GUID: let the old one go first.
        let start = std::time::Instant::now();
        while luid(TUN_NAME).is_some() && start.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        crate::os::spawn_core(cmd).map(|child| Running { child, pid_file: None })
    }

    /// The core has routed the internet into the adapter.
    pub fn ready(&self) -> bool {
        let Some(tun) = luid(TUN_NAME) else { return false };
        routes(|r| r.DestinationPrefix.PrefixLength == 1 && unsafe { r.InterfaceLuid.Value } == tun)
    }

    /// True when the uplink changed.
    pub fn tick(&mut self) -> bool {
        let now = uplink();
        let changed = now.is_some() && now != self.uplink;
        if changed {
            self.uplink = now;
        }
        changed
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn luid(alias: &str) -> Option<u64> {
    let mut luid = NET_LUID_LH::default();
    // SAFETY: a NUL-terminated name and an out parameter.
    unsafe { ConvertInterfaceAliasToLuid(PCWSTR(wide(alias).as_ptr()), &mut luid).is_ok().then(|| luid.Value) }
}

/// Whether any IPv4 route matches `f`.
fn routes(mut f: impl FnMut(&MIB_IPFORWARD_ROW2) -> bool) -> bool {
    let mut table: *mut MIB_IPFORWARD_TABLE2 = std::ptr::null_mut();
    // SAFETY: the table is read within its length and freed once.
    unsafe {
        if GetIpForwardTable2(AF_INET, &mut table).is_err() {
            return false;
        }
        let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
        let found = rows.iter().any(&mut f);
        FreeMibTable(table as *const _);
        found
    }
}

/// The interface of the best IPv4 default route outside the TUN, by its alias (what Xray calls the interface).
fn uplink() -> Option<String> {
    let tun = luid(TUN_NAME);
    let mut best: Option<(u32, NET_LUID_LH)> = None;
    routes(|r| {
        // SAFETY: plain reads of the row and an interface lookup into a row we own.
        unsafe {
            if r.DestinationPrefix.PrefixLength == 0 && Some(r.InterfaceLuid.Value) != tun {
                let mut ip = MIB_IPINTERFACE_ROW::default();
                InitializeIpInterfaceEntry(&mut ip);
                ip.Family = AF_INET;
                ip.InterfaceLuid = r.InterfaceLuid;
                if GetIpInterfaceEntry(&mut ip).is_ok() && best.is_none_or(|(m, _)| r.Metric + ip.Metric < m) {
                    best = Some((r.Metric + ip.Metric, r.InterfaceLuid));
                }
            }
        }
        false
    });
    let mut alias = [0u16; 257];
    // SAFETY: the buffer holds the longest alias (IF_MAX_STRING_SIZE + 1).
    unsafe { ConvertInterfaceLuidToAlias(&best?.1, &mut alias).is_ok() }.then(|| String::from_utf16_lossy(&alias[..alias.iter().position(|c| *c == 0).unwrap_or(0)]))
}

/// Local and link-scoped destinations the kill switch leaves alone, like on Linux: (address, prefix).
const LAN4: [(Ipv4Addr, u32); 7] = [
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::BROADCAST, 32),
];
const LAN6: [(Ipv6Addr, u8); 3] =
    [(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7), (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10), (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8)];
const LAYERS: [GUID; 2] = [FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6];

fn condition(field: GUID, match_type: FWP_MATCH_TYPE, r#type: FWP_DATA_TYPE, value: FWP_CONDITION_VALUE0_0) -> FWPM_FILTER_CONDITION0 {
    FWPM_FILTER_CONDITION0 { fieldKey: field, matchType: match_type, conditionValue: FWP_CONDITION_VALUE0 { r#type, Anonymous: value } }
}

fn port(field: GUID, port: u16) -> FWPM_FILTER_CONDITION0 {
    condition(field, FWP_MATCH_EQUAL, FWP_UINT16, FWP_CONDITION_VALUE0_0 { uint16: port })
}

fn udp() -> FWPM_FILTER_CONDITION0 {
    condition(FWPM_CONDITION_IP_PROTOCOL, FWP_MATCH_EQUAL, FWP_UINT8, FWP_CONDITION_VALUE0_0 { uint8: 17 })
}

/// Our sublayer in a dynamic WFP session. In it the highest weight that matches decides.
/// The sublayer's key is new for every session: one of a helper that has not ended yet must not block this one.
struct Wfp {
    engine: HANDLE,
    sublayer: GUID,
}

impl Wfp {
    fn open() -> Result<Wfp, String> {
        let mut name = wide("Hitori VPN");
        let display = FWPM_DISPLAY_DATA0 { name: PWSTR(name.as_mut_ptr()), description: PWSTR::null() };
        let session = FWPM_SESSION0 { displayData: display, flags: FWPM_SESSION_FLAG_DYNAMIC, ..Default::default() };
        let sublayer = random_guid()?;
        let mut engine = HANDLE::default();
        // SAFETY: every pointer refers to locals alive across the calls; the engine is closed on drop.
        unsafe {
            check(FwpmEngineOpen0(PCWSTR::null(), RPC_C_AUTHN_WINNT, None, Some(&session), &mut engine), "open the Windows Filtering Platform")?;
            let wfp = Wfp { engine, sublayer };
            let sublayer = FWPM_SUBLAYER0 { subLayerKey: wfp.sublayer, displayData: display, weight: u16::MAX, ..Default::default() };
            check(FwpmSubLayerAdd0(engine, &sublayer, None), "add a WFP sublayer")?;
            Ok(wfp)
        }
    }

    fn add(&self, layer: GUID, weight: u8, action: FWP_ACTION_TYPE, conditions: &[FWPM_FILTER_CONDITION0]) -> Result<u64, String> {
        let mut name = wide("Hitori VPN");
        let filter = FWPM_FILTER0 {
            displayData: FWPM_DISPLAY_DATA0 { name: PWSTR(name.as_mut_ptr()), description: PWSTR::null() },
            layerKey: layer,
            subLayerKey: self.sublayer,
            weight: FWP_VALUE0 { r#type: FWP_UINT8, Anonymous: FWP_VALUE0_0 { uint8: weight } },
            numFilterConditions: conditions.len() as u32,
            filterCondition: if conditions.is_empty() { std::ptr::null_mut() } else { conditions.as_ptr() as *mut _ },
            action: FWPM_ACTION0 { r#type: action, ..Default::default() },
            ..Default::default()
        };
        let mut id = 0;
        // SAFETY: the filter and what its conditions point to live across the call, which copies them.
        check(unsafe { FwpmFilterAdd0(self.engine, &filter, None, Some(&mut id)) }, "add a WFP filter")?;
        Ok(id)
    }

    fn delete(&self, id: u64) {
        // SAFETY: an id this session added.
        unsafe { FwpmFilterDeleteById0(self.engine, id) };
    }

    /// Always in TUN mode: DNS only through the TUN (or from the core), so no resolver outside it sees queries.
    fn base(&self, xray: &Path) -> Result<(), String> {
        let mut app: *mut FWP_BYTE_BLOB = std::ptr::null_mut();
        // SAFETY: the blob is allocated by WFP and freed below.
        check(unsafe { FwpmGetAppIdFromFileName0(PCWSTR(wide(&xray.to_string_lossy()).as_ptr()), &mut app) }, "identify the core")?;
        let tun4 = u32::from(tun_addr(0).parse::<Ipv4Addr>().map_err(|e| e.to_string())?);
        let mut tun6 = FWP_BYTE_ARRAY16 { byteArray16: tun_addr(1).parse::<Ipv6Addr>().map_err(|e| e.to_string())?.octets() };
        let result = (|| {
            for (i, layer) in LAYERS.into_iter().enumerate() {
                let core = condition(FWPM_CONDITION_ALE_APP_ID, FWP_MATCH_EQUAL, FWP_BYTE_BLOB_TYPE, FWP_CONDITION_VALUE0_0 { byteBlob: app });
                let loopback = condition(FWPM_CONDITION_FLAGS, FWP_MATCH_FLAGS_ALL_SET, FWP_UINT32, FWP_CONDITION_VALUE0_0 { uint32: FWP_CONDITION_FLAG_IS_LOOPBACK });
                let tun = if i == 0 {
                    condition(FWPM_CONDITION_IP_LOCAL_ADDRESS, FWP_MATCH_EQUAL, FWP_UINT32, FWP_CONDITION_VALUE0_0 { uint32: tun4 })
                } else {
                    condition(FWPM_CONDITION_IP_LOCAL_ADDRESS, FWP_MATCH_EQUAL, FWP_BYTE_ARRAY16_TYPE, FWP_CONDITION_VALUE0_0 { byteArray16: &mut tun6 })
                };
                self.add(layer, 15, FWP_ACTION_PERMIT, &[core])?;
                self.add(layer, 14, FWP_ACTION_PERMIT, &[loopback])?;
                self.add(layer, 13, FWP_ACTION_PERMIT, &[tun])?;
                self.add(layer, 12, FWP_ACTION_BLOCK, &[port(FWPM_CONDITION_IP_REMOTE_PORT, 53)])?;
            }
            Ok(())
        })();
        // SAFETY: allocated by FwpmGetAppIdFromFileName0.
        unsafe { FwpmFreeMemory0(&mut app as *mut _ as *mut *mut _) };
        result
    }

    /// Below the permits above: DHCP and the LAN still work, everything else is blocked. Adds the filter ids to `ids`.
    fn kill_switch(&self, ids: &mut Vec<u64>) -> Result<(), String> {
        let mut lan4: Vec<FWP_V4_ADDR_AND_MASK> = LAN4.iter().map(|(a, p)| FWP_V4_ADDR_AND_MASK { addr: u32::from(*a), mask: u32::MAX.checked_shl(32 - p).unwrap_or(0) }).collect();
        let mut lan6: Vec<FWP_V6_ADDR_AND_MASK> = LAN6.iter().map(|(a, p)| FWP_V6_ADDR_AND_MASK { addr: a.octets(), prefixLength: *p }).collect();
        let v4: Vec<_> =
            lan4.iter_mut().map(|m| condition(FWPM_CONDITION_IP_REMOTE_ADDRESS, FWP_MATCH_EQUAL, FWP_V4_ADDR_MASK, FWP_CONDITION_VALUE0_0 { v4AddrMask: m })).collect();
        let v6: Vec<_> =
            lan6.iter_mut().map(|m| condition(FWPM_CONDITION_IP_REMOTE_ADDRESS, FWP_MATCH_EQUAL, FWP_V6_ADDR_MASK, FWP_CONDITION_VALUE0_0 { v6AddrMask: m })).collect();
        let mut add = |layer, weight, action, conditions: &[FWPM_FILTER_CONDITION0]| -> Result<(), String> {
            ids.push(self.add(layer, weight, action, conditions)?);
            Ok(())
        };
        let [l4, l6] = LAYERS;
        // Conditions on the same field match any of their values.
        add(l4, 11, FWP_ACTION_PERMIT, &v4)?;
        add(l6, 11, FWP_ACTION_PERMIT, &v6)?;
        add(l4, 11, FWP_ACTION_PERMIT, &[udp(), port(FWPM_CONDITION_IP_LOCAL_PORT, 68), port(FWPM_CONDITION_IP_REMOTE_PORT, 67)])?;
        add(l6, 11, FWP_ACTION_PERMIT, &[udp(), port(FWPM_CONDITION_IP_LOCAL_PORT, 546), port(FWPM_CONDITION_IP_REMOTE_PORT, 547)])?;
        add(l4, 10, FWP_ACTION_BLOCK, &[])?;
        add(l6, 10, FWP_ACTION_BLOCK, &[])
    }
}

impl Drop for Wfp {
    fn drop(&mut self) {
        // SAFETY: closing the dynamic session removes every filter it added.
        unsafe { FwpmEngineClose0(self.engine) };
    }
}

fn random_guid() -> Result<GUID, String> {
    let mut bytes = [0u8; 16];
    // SAFETY: fills the buffer it is given.
    unsafe { BCryptGenRandom(None, &mut bytes, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().map_err(|e| format!("cannot make a WFP key: {e}"))?;
    Ok(GUID::from_u128(u128::from_le_bytes(bytes)))
}

/// Address `i` of TUN_ADDRS without its prefix.
fn tun_addr(i: usize) -> &'static str {
    TUN_ADDRS[i].split('/').next().unwrap()
}

fn check(code: u32, what: &str) -> Result<(), String> {
    match code {
        0 => Ok(()),
        5 => Err(format!("cannot {what}: administrator rights are needed")),
        _ => {
            let hr = if code & 0x8000_0000 == 0 { windows::core::HRESULT::from_win32(code) } else { windows::core::HRESULT(code as i32) };
            Err(format!("cannot {what}: {} ({code:#x})", hr.message()))
        }
    }
}
