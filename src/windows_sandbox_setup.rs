#![allow(unsafe_op_in_unsafe_fn)]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::Command;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Foundation::VARIANT_TRUE;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwPolicy2;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwRule3;
use windows::Win32::NetworkManagement::WindowsFirewall::INetFwRules;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_ACTION_BLOCK;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_ANY;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_TCP;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_IP_PROTOCOL_UDP;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_MODIFY_STATE;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_MODIFY_STATE_OK;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_PROFILE2_ALL;
use windows::Win32::NetworkManagement::WindowsFirewall::NET_FW_RULE_DIR_OUT;
use windows::Win32::NetworkManagement::WindowsFirewall::NetFwPolicy2;
use windows::Win32::NetworkManagement::WindowsFirewall::NetFwRule;
use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::Com::CoCreateInstance;
use windows::Win32::System::Com::CoInitializeEx;
use windows::Win32::System::Com::CoUninitialize;
use windows::core::BSTR;
use windows::core::Interface;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Foundation::SetHandleInformation;
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::Security::Cryptography::BCRYPT_USE_SYSTEM_PREFERRED_RNG;
use windows_sys::Win32::Security::Cryptography::BCryptGenRandom;
use windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;
use windows_sys::Win32::Security::Cryptography::CRYPTPROTECT_UI_FORBIDDEN;
use windows_sys::Win32::Security::Cryptography::CryptProtectData;
use windows_sys::Win32::Security::Cryptography::CryptUnprotectData;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::TOKEN_ELEVATION;
use windows_sys::Win32::Security::TOKEN_QUERY;
use windows_sys::Win32::Security::TokenElevation;
use windows_sys::Win32::System::Console::GetStdHandle;
use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
use windows_sys::Win32::System::Console::STD_INPUT_HANDLE;
use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
use windows_sys::Win32::System::JobObjects::CreateJobObjectW;
use windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
use windows_sys::Win32::System::JobObjects::JOBOBJECT_EXTENDED_LIMIT_INFORMATION;
use windows_sys::Win32::System::JobObjects::JobObjectExtendedLimitInformation;
use windows_sys::Win32::System::JobObjects::SetInformationJobObject;
use windows_sys::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT;
use windows_sys::Win32::System::Threading::CreateProcessWithLogonW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::INFINITE;
use windows_sys::Win32::System::Threading::LOGON_WITH_PROFILE;
use windows_sys::Win32::System::Threading::OpenProcessToken;
use windows_sys::Win32::System::Threading::PROCESS_INFORMATION;
use windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
use windows_sys::Win32::System::Threading::STARTUPINFOW;
use windows_sys::Win32::System::Threading::WaitForSingleObject;

pub const OFFLINE_USERNAME: &str = "McpReplOffline";
pub const DEFAULT_HTTP_PROXY_PORT: u16 = 39080;
pub const DEFAULT_SOCKS_PROXY_PORT: u16 = 39081;

const SETUP_VERSION: u32 = 1;
const SETUP_DIR_NAME: &str = "mcp-repl\\windows-sandbox";
const SETUP_MARKER_FILE: &str = "setup_marker.json";
const HANDLE_FLAG_INHERIT: u32 = 0x00000001;
const LOOPBACK_REMOTE_ADDRESSES: &str = "127.0.0.0/8,::/127";
const NON_LOOPBACK_REMOTE_ADDRESSES: &str = "0.0.0.0-126.255.255.255,128.0.0.0-255.255.255.255,::,::2-ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff";
const OFFLINE_BLOCK_RULE_NAME: &str = "mcp_repl_sandbox_offline_block_outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME: &str = "mcp_repl_sandbox_offline_block_loopback_tcp";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME: &str = "mcp_repl_sandbox_offline_block_loopback_udp";
const OFFLINE_BLOCK_RULE_FRIENDLY: &str = "mcp-repl Offline Sandbox - Block Non-Loopback Outbound";
const OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY: &str =
    "mcp-repl Offline Sandbox - Block Loopback TCP Except Proxy";
const OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY: &str =
    "mcp-repl Offline Sandbox - Block Loopback UDP";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSandboxSetupOptions {
    pub http_proxy_port: u16,
    pub socks_proxy_port: u16,
}

impl Default for WindowsSandboxSetupOptions {
    fn default() -> Self {
        Self {
            http_proxy_port: DEFAULT_HTTP_PROXY_PORT,
            socks_proxy_port: DEFAULT_SOCKS_PROXY_PORT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsSandboxOfflineSetup {
    pub username: String,
    pub user_sid: String,
    pub http_proxy_port: u16,
    pub socks_proxy_port: u16,
}

#[derive(Debug, Clone)]
pub struct WindowsSandboxOfflineCredentials {
    pub setup: WindowsSandboxOfflineSetup,
    pub password: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct SetupMarker {
    version: u32,
    username: String,
    user_sid: String,
    http_proxy_port: u16,
    socks_proxy_port: u16,
    password_dpapi_b64: String,
}

struct BlockRuleSpec<'a> {
    internal_name: &'a str,
    friendly_desc: &'a str,
    protocol: i32,
    local_user_spec: &'a str,
    offline_sid: &'a str,
    remote_addresses: Option<&'a str>,
    remote_ports: Option<&'a str>,
}

pub fn parse_setup_args(args: &[String]) -> Result<WindowsSandboxSetupOptions, String> {
    let mut options = WindowsSandboxSetupOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                print_setup_usage();
                std::process::exit(0);
            }
            "--http-proxy-port" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "missing value for --http-proxy-port".to_string())?;
                options.http_proxy_port = parse_port(value, "--http-proxy-port")?;
            }
            arg if arg.starts_with("--http-proxy-port=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                options.http_proxy_port = parse_port(value, "--http-proxy-port")?;
            }
            "--socks-proxy-port" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "missing value for --socks-proxy-port".to_string())?;
                options.socks_proxy_port = parse_port(value, "--socks-proxy-port")?;
            }
            arg if arg.starts_with("--socks-proxy-port=") => {
                let value = arg.split_once('=').map(|(_, value)| value).unwrap_or("");
                options.socks_proxy_port = parse_port(value, "--socks-proxy-port")?;
            }
            other => return Err(format!("unknown windows-sandbox setup option: {other}")),
        }
        index += 1;
    }
    validate_setup_options(&options)?;
    Ok(options)
}

pub fn print_setup_usage() {
    println!(
        "Usage:\n\
mcp-repl windows-sandbox setup [--http-proxy-port <port>] [--socks-proxy-port <port>]\n\n\
Creates or refreshes the Windows offline sandbox account, DPAPI-protected credentials,\n\
and account-scoped firewall rules. Run from an elevated shell as the user who will run mcp-repl."
    );
}

pub fn run_setup(options: WindowsSandboxSetupOptions) -> Result<(), String> {
    validate_setup_options(&options)?;
    if !is_process_elevated()? {
        return Err(
            "windows sandbox setup must be run from an elevated shell as the user who will run mcp-repl"
                .to_string(),
        );
    }

    let password = generate_password()?;
    let user_sid = ensure_offline_user(&password)?;
    ensure_offline_firewall_rules(
        &user_sid,
        &[options.http_proxy_port, options.socks_proxy_port],
    )?;
    write_setup_marker(&SetupMarker {
        version: SETUP_VERSION,
        username: OFFLINE_USERNAME.to_string(),
        user_sid: user_sid.clone(),
        http_proxy_port: options.http_proxy_port,
        socks_proxy_port: options.socks_proxy_port,
        password_dpapi_b64: protect_password(&password)?,
    })?;

    println!(
        "Windows sandbox setup complete for {OFFLINE_USERNAME} ({user_sid}); HTTP proxy port {}, SOCKS proxy port {}.",
        options.http_proxy_port, options.socks_proxy_port
    );
    Ok(())
}

pub fn load_offline_setup() -> Result<WindowsSandboxOfflineSetup, String> {
    let marker = read_setup_marker()?;
    validate_marker(&marker)?;
    Ok(WindowsSandboxOfflineSetup {
        username: marker.username,
        user_sid: marker.user_sid,
        http_proxy_port: marker.http_proxy_port,
        socks_proxy_port: marker.socks_proxy_port,
    })
}

pub fn load_offline_credentials() -> Result<WindowsSandboxOfflineCredentials, String> {
    let marker = read_setup_marker()?;
    validate_marker(&marker)?;
    let password = unprotect_password(&marker.password_dpapi_b64)?;
    Ok(WindowsSandboxOfflineCredentials {
        setup: WindowsSandboxOfflineSetup {
            username: marker.username,
            user_sid: marker.user_sid,
            http_proxy_port: marker.http_proxy_port,
            socks_proxy_port: marker.socks_proxy_port,
        },
        password,
    })
}

pub fn missing_setup_message() -> String {
    format!(
        "Windows proxy-enforced sandbox setup is required. Run from an elevated shell: mcp-repl windows-sandbox setup --http-proxy-port {DEFAULT_HTTP_PROXY_PORT} --socks-proxy-port {DEFAULT_SOCKS_PROXY_PORT}"
    )
}

pub fn run_offline_logon_wrapper(child_args: Vec<String>) -> Result<i32, String> {
    if child_args.is_empty() {
        return Err("offline logon wrapper requires child arguments".to_string());
    }
    let credentials =
        load_offline_credentials().map_err(|err| format!("{} ({err})", missing_setup_message()))?;
    create_process_with_offline_logon(&credentials, child_args)
}

fn parse_port(raw: &str, flag: &str) -> Result<u16, String> {
    let value = raw
        .parse::<u16>()
        .map_err(|_| format!("invalid value for {flag}: {raw}"))?;
    if value == 0 {
        return Err(format!("invalid value for {flag}: 0"));
    }
    Ok(value)
}

fn validate_setup_options(options: &WindowsSandboxSetupOptions) -> Result<(), String> {
    if options.http_proxy_port == options.socks_proxy_port {
        return Err("HTTP and SOCKS proxy ports must be different".to_string());
    }
    Ok(())
}

fn setup_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or_else(|| "LOCALAPPDATA is not set; cannot store Windows sandbox setup".to_string())?;
    Ok(base.join(SETUP_DIR_NAME))
}

fn setup_marker_path() -> Result<PathBuf, String> {
    Ok(setup_dir()?.join(SETUP_MARKER_FILE))
}

fn read_setup_marker() -> Result<SetupMarker, String> {
    let path = setup_marker_path()?;
    let text = fs::read_to_string(&path).map_err(|err| {
        format!(
            "{} (failed to read '{}': {err})",
            missing_setup_message(),
            path.display()
        )
    })?;
    serde_json::from_str(&text)
        .map_err(|err| format!("failed to parse Windows sandbox setup marker: {err}"))
}

fn write_setup_marker(marker: &SetupMarker) -> Result<(), String> {
    let dir = setup_dir()?;
    fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create Windows sandbox setup dir: {err}"))?;
    let path = setup_marker_path()?;
    let text = serde_json::to_string_pretty(marker)
        .map_err(|err| format!("failed to serialize Windows sandbox setup marker: {err}"))?;
    fs::write(&path, text)
        .map_err(|err| format!("failed to write Windows sandbox setup marker: {err}"))?;
    Ok(())
}

fn validate_marker(marker: &SetupMarker) -> Result<(), String> {
    if marker.version != SETUP_VERSION {
        return Err(format!(
            "{} (setup marker version {} is not supported)",
            missing_setup_message(),
            marker.version
        ));
    }
    if marker.username != OFFLINE_USERNAME {
        return Err(format!(
            "{} (setup marker is for unexpected user {})",
            missing_setup_message(),
            marker.username
        ));
    }
    validate_setup_options(&WindowsSandboxSetupOptions {
        http_proxy_port: marker.http_proxy_port,
        socks_proxy_port: marker.socks_proxy_port,
    })
}

fn generate_password() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            bytes.len() as u32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(format!(
            "BCryptGenRandom failed while creating sandbox password with NTSTATUS 0x{status:08x}"
        ));
    }
    Ok(format!(
        "McpRepl-{}!",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn protect_password(password: &str) -> Result<String, String> {
    let input = CRYPT_INTEGER_BLOB {
        cbData: password.len() as u32,
        pbData: password.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &input,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptProtectData failed for Windows sandbox password: {}",
            io::Error::last_os_error()
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    unsafe {
        let _ = LocalFree(output.pbData as HLOCAL);
    }
    Ok(encoded)
}

fn unprotect_password(encoded: &str) -> Result<String, String> {
    let encrypted = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|err| format!("Windows sandbox password is not valid base64: {err}"))?;
    let input = CRYPT_INTEGER_BLOB {
        cbData: encrypted.len() as u32,
        pbData: encrypted.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptUnprotectData failed for Windows sandbox password: {}",
            io::Error::last_os_error()
        ));
    }
    let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let password = String::from_utf8(bytes.to_vec())
        .map_err(|err| format!("Windows sandbox password is not valid UTF-8: {err}"))?;
    unsafe {
        let _ = LocalFree(output.pbData as HLOCAL);
    }
    Ok(password)
}

fn is_process_elevated() -> Result<bool, String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(format!(
                "OpenProcessToken failed while checking elevation: {}",
                io::Error::last_os_error()
            ));
        }
        let mut elevation: TOKEN_ELEVATION = std::mem::zeroed();
        let mut returned = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            &mut elevation as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        );
        CloseHandle(token);
        if ok == 0 {
            return Err(format!(
                "GetTokenInformation(TokenElevation) failed: {}",
                io::Error::last_os_error()
            ));
        }
        Ok(elevation.TokenIsElevated != 0)
    }
}

fn ensure_offline_user(password: &str) -> Result<String, String> {
    let escaped_password = powershell_single_quote(password);
    let escaped_name = powershell_single_quote(OFFLINE_USERNAME);
    let script = format!(
        r#"
$ErrorActionPreference = 'Stop'
$name = '{escaped_name}'
$password = ConvertTo-SecureString -String '{escaped_password}' -AsPlainText -Force
$user = Get-LocalUser -Name $name -ErrorAction SilentlyContinue
if ($null -eq $user) {{
  New-LocalUser -Name $name -Password $password -AccountNeverExpires -UserMayNotChangePassword -Description 'mcp-repl offline sandbox user' | Out-Null
}} else {{
  Enable-LocalUser -Name $name
  Set-LocalUser -Name $name -Password $password
}}
try {{ Set-LocalUser -Name $name -PasswordNeverExpires $true }} catch {{ }}
$user = Get-LocalUser -Name $name
$user.SID.Value
"#
    );
    let sid = run_powershell_script(&script)?;
    let sid = sid.trim();
    if sid.is_empty() || !sid.starts_with("S-1-") {
        return Err(format!(
            "failed to resolve SID for {OFFLINE_USERNAME}: {sid:?}"
        ));
    }
    Ok(sid.to_string())
}

fn powershell_single_quote(value: &str) -> String {
    value.replace('\'', "''")
}

fn run_powershell_script(script: &str) -> Result<String, String> {
    let mut utf16 = Vec::new();
    for value in script.encode_utf16() {
        utf16.extend_from_slice(&value.to_le_bytes());
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
    let output = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-EncodedCommand",
            &encoded,
        ])
        .output()
        .map_err(|err| format!("failed to run PowerShell for Windows sandbox setup: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "PowerShell Windows sandbox setup failed with status {}: {}{}",
            output.status, stdout, stderr
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn ensure_offline_firewall_rules(offline_sid: &str, proxy_ports: &[u16]) -> Result<(), String> {
    let local_user_spec = format!("O:LSD:(A;;CC;;;{offline_sid})");
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!(
            "CoInitializeEx failed for Windows Firewall COM: {hr:?}"
        ));
    }

    let result = unsafe {
        (|| -> Result<(), String> {
            let policy: INetFwPolicy2 = CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| format!("CoCreateInstance NetFwPolicy2 failed: {err:?}"))?;
            ensure_local_policy_rules_take_effect(&policy)?;
            let rules = policy
                .Rules()
                .map_err(|err| format!("INetFwPolicy2::Rules failed: {err:?}"))?;

            ensure_block_rule(
                &rules,
                &BlockRuleSpec {
                    internal_name: OFFLINE_BLOCK_RULE_NAME,
                    friendly_desc: OFFLINE_BLOCK_RULE_FRIENDLY,
                    protocol: NET_FW_IP_PROTOCOL_ANY.0,
                    local_user_spec: &local_user_spec,
                    offline_sid,
                    remote_addresses: Some(NON_LOOPBACK_REMOTE_ADDRESSES),
                    remote_ports: None,
                },
            )?;
            ensure_block_rule(
                &rules,
                &BlockRuleSpec {
                    internal_name: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_NAME,
                    friendly_desc: OFFLINE_BLOCK_LOOPBACK_UDP_RULE_FRIENDLY,
                    protocol: NET_FW_IP_PROTOCOL_UDP.0,
                    local_user_spec: &local_user_spec,
                    offline_sid,
                    remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                    remote_ports: None,
                },
            )?;
            let blocked_remote_ports = blocked_loopback_tcp_remote_ports(proxy_ports);
            ensure_block_rule(
                &rules,
                &BlockRuleSpec {
                    internal_name: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_NAME,
                    friendly_desc: OFFLINE_BLOCK_LOOPBACK_TCP_RULE_FRIENDLY,
                    protocol: NET_FW_IP_PROTOCOL_TCP.0,
                    local_user_spec: &local_user_spec,
                    offline_sid,
                    remote_addresses: Some(LOOPBACK_REMOTE_ADDRESSES),
                    remote_ports: blocked_remote_ports.as_deref(),
                },
            )?;
            Ok(())
        })()
    };
    unsafe {
        CoUninitialize();
    }
    result
}

unsafe fn ensure_local_policy_rules_take_effect(policy: &INetFwPolicy2) -> Result<(), String> {
    let mut modify_state = NET_FW_MODIFY_STATE::default();
    let result = (Interface::vtable(policy).LocalPolicyModifyState)(
        Interface::as_raw(policy),
        &mut modify_state,
    );
    if result.is_err() {
        return Err(format!(
            "INetFwPolicy2::LocalPolicyModifyState failed: {result:?}"
        ));
    }
    if result != S_OK {
        return Err(format!(
            "local firewall policy modifications do not apply to every active profile: result={result:?}"
        ));
    }
    if modify_state != NET_FW_MODIFY_STATE_OK {
        return Err(format!(
            "local firewall policy modifications will not take effect: LocalPolicyModifyState={modify_state:?}"
        ));
    }
    Ok(())
}

unsafe fn ensure_block_rule(rules: &INetFwRules, spec: &BlockRuleSpec<'_>) -> Result<(), String> {
    let name = BSTR::from(spec.internal_name);
    let rule: INetFwRule3 = match rules.Item(&name) {
        Ok(existing) => existing
            .cast()
            .map_err(|err| format!("cast existing firewall rule to INetFwRule3 failed: {err:?}"))?,
        Err(_) => {
            let new_rule: INetFwRule3 = CoCreateInstance(&NetFwRule, None, CLSCTX_INPROC_SERVER)
                .map_err(|err| format!("CoCreateInstance NetFwRule failed: {err:?}"))?;
            new_rule
                .SetName(&name)
                .map_err(|err| format!("SetName failed: {err:?}"))?;
            configure_rule(&new_rule, spec)?;
            rules
                .Add(&new_rule)
                .map_err(|err| format!("Rules::Add failed: {err:?}"))?;
            new_rule
        }
    };
    configure_rule(&rule, spec)
}

unsafe fn configure_rule(rule: &INetFwRule3, spec: &BlockRuleSpec<'_>) -> Result<(), String> {
    rule.SetDescription(&BSTR::from(spec.friendly_desc))
        .map_err(|err| format!("SetDescription failed: {err:?}"))?;
    rule.SetDirection(NET_FW_RULE_DIR_OUT)
        .map_err(|err| format!("SetDirection failed: {err:?}"))?;
    rule.SetAction(NET_FW_ACTION_BLOCK)
        .map_err(|err| format!("SetAction failed: {err:?}"))?;
    rule.SetEnabled(VARIANT_TRUE)
        .map_err(|err| format!("SetEnabled failed: {err:?}"))?;
    rule.SetProfiles(NET_FW_PROFILE2_ALL.0)
        .map_err(|err| format!("SetProfiles failed: {err:?}"))?;
    rule.SetProtocol(spec.protocol)
        .map_err(|err| format!("SetProtocol failed: {err:?}"))?;
    rule.SetRemoteAddresses(&BSTR::from(spec.remote_addresses.unwrap_or("*")))
        .map_err(|err| format!("SetRemoteAddresses failed: {err:?}"))?;
    if let Some(remote_ports) = spec.remote_ports {
        rule.SetRemotePorts(&BSTR::from(remote_ports))
            .map_err(|err| format!("SetRemotePorts failed: {err:?}"))?;
    }
    rule.SetLocalUserAuthorizedList(&BSTR::from(spec.local_user_spec))
        .map_err(|err| format!("SetLocalUserAuthorizedList failed: {err:?}"))?;

    let actual = rule
        .LocalUserAuthorizedList()
        .map_err(|err| format!("LocalUserAuthorizedList read-back failed: {err:?}"))?
        .to_string();
    if !actual.contains(spec.offline_sid) {
        return Err(format!(
            "offline firewall rule user scope mismatch: expected SID {}, got {actual}",
            spec.offline_sid
        ));
    }
    Ok(())
}

pub fn blocked_loopback_tcp_remote_ports(proxy_ports: &[u16]) -> Option<String> {
    let mut allowed_ports = proxy_ports
        .iter()
        .copied()
        .filter(|port| *port != 0)
        .collect::<Vec<_>>();
    allowed_ports.sort_unstable();
    allowed_ports.dedup();

    let mut blocked_ranges = Vec::new();
    let mut start = 1_u32;
    for port in allowed_ports {
        let port = u32::from(port);
        if port < start {
            continue;
        }
        if port > start {
            blocked_ranges.push(port_range_string(start, port - 1));
        }
        start = port + 1;
    }

    if start <= u32::from(u16::MAX) {
        blocked_ranges.push(port_range_string(start, u32::from(u16::MAX)));
    }
    (!blocked_ranges.is_empty()).then(|| blocked_ranges.join(","))
}

fn port_range_string(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
}

fn create_process_with_offline_logon(
    credentials: &WindowsSandboxOfflineCredentials,
    child_args: Vec<String>,
) -> Result<i32, String> {
    unsafe {
        let current_exe = std::env::current_exe()
            .map_err(|err| format!("failed to resolve current executable: {err}"))?;
        let mut argv = vec![current_exe.to_string_lossy().to_string()];
        argv.extend(child_args);
        let cmdline = argv
            .iter()
            .map(|arg| quote_windows_arg(arg))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmdline = to_wide(&cmdline);
        let app = to_wide(&current_exe);
        let username = to_wide(&credentials.setup.username);
        let domain = to_wide(".");
        let password = to_wide(&credentials.password);
        let cwd = std::env::current_dir()
            .map_err(|err| format!("failed to resolve current directory: {err}"))?;
        let cwd = to_wide(cwd);
        let env_block = make_env_block(&std::env::vars().collect());
        let mut startup_info: STARTUPINFOW = std::mem::zeroed();
        startup_info.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        ensure_inheritable_stdio(&mut startup_info)?;
        let mut proc_info: PROCESS_INFORMATION = std::mem::zeroed();
        let ok = CreateProcessWithLogonW(
            username.as_ptr(),
            domain.as_ptr(),
            password.as_ptr(),
            LOGON_WITH_PROFILE,
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            CREATE_UNICODE_ENVIRONMENT,
            env_block.as_ptr() as *const c_void,
            cwd.as_ptr(),
            &startup_info,
            &mut proc_info,
        );
        if ok == 0 {
            return Err(format!(
                "CreateProcessWithLogonW failed for {}: {}",
                credentials.setup.username,
                io::Error::last_os_error()
            ));
        }
        let job_handle = create_job_kill_on_close().ok();
        if let Some(job) = job_handle {
            let _ = AssignProcessToJobObject(job, proc_info.hProcess);
        }
        let wait_status = WaitForSingleObject(proc_info.hProcess, INFINITE);
        if wait_status == WAIT_FAILED {
            if let Some(job) = job_handle {
                CloseHandle(job);
            }
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            return Err(format!(
                "WaitForSingleObject failed for offline sandbox wrapper: {}",
                GetLastError()
            ));
        }
        let mut exit_code = 1u32;
        if GetExitCodeProcess(proc_info.hProcess, &mut exit_code) == 0 {
            if let Some(job) = job_handle {
                CloseHandle(job);
            }
            CloseHandle(proc_info.hThread);
            CloseHandle(proc_info.hProcess);
            return Err(format!(
                "GetExitCodeProcess failed for offline sandbox wrapper: {}",
                io::Error::last_os_error()
            ));
        }
        if let Some(job) = job_handle {
            CloseHandle(job);
        }
        CloseHandle(proc_info.hThread);
        CloseHandle(proc_info.hProcess);
        Ok(exit_code as i32)
    }
}

unsafe fn create_job_kill_on_close() -> Result<HANDLE, String> {
    let handle = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
    if handle.is_null() {
        return Err("CreateJobObjectW failed for offline sandbox wrapper".to_string());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = SetInformationJobObject(
        handle,
        JobObjectExtendedLimitInformation,
        &mut limits as *mut _ as *mut _,
        std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    );
    if ok == 0 {
        CloseHandle(handle);
        return Err("SetInformationJobObject failed for offline sandbox wrapper".to_string());
    }
    Ok(handle)
}

unsafe fn ensure_inheritable_stdio(startup_info: &mut STARTUPINFOW) -> Result<(), String> {
    for std_handle_kind in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let std_handle = GetStdHandle(std_handle_kind);
        if std_handle.is_null() || std_handle == INVALID_HANDLE_VALUE {
            return Err(format!(
                "GetStdHandle failed for offline sandbox wrapper: {}",
                io::Error::last_os_error()
            ));
        }
        if SetHandleInformation(std_handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
            return Err(format!(
                "SetHandleInformation failed for offline sandbox wrapper: {}",
                io::Error::last_os_error()
            ));
        }
    }
    startup_info.dwFlags |= STARTF_USESTDHANDLES;
    startup_info.hStdInput = GetStdHandle(STD_INPUT_HANDLE);
    startup_info.hStdOutput = GetStdHandle(STD_OUTPUT_HANDLE);
    startup_info.hStdError = GetStdHandle(STD_ERROR_HANDLE);
    Ok(())
}

fn make_env_block(env: &HashMap<String, String>) -> Vec<u16> {
    let mut items: Vec<(String, String)> = env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    items.sort_by(|a, b| {
        a.0.to_uppercase()
            .cmp(&b.0.to_uppercase())
            .then(a.0.cmp(&b.0))
    });
    let mut wide_env = Vec::new();
    for (key, value) in items {
        let mut entry = to_wide(format!("{key}={value}"));
        entry.pop();
        wide_env.extend_from_slice(&entry);
        wide_env.push(0);
    }
    wide_env.push(0);
    wide_env
}

fn to_wide<S: AsRef<OsStr>>(value: S) -> Vec<u16> {
    let mut wide: Vec<u16> = value.as_ref().encode_wide().collect();
    wide.push(0);
    wide
}

fn quote_windows_arg(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\r' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }

    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('"');
    let mut backslashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                if backslashes > 0 {
                    quoted.push_str(&"\\".repeat(backslashes));
                    backslashes = 0;
                }
                quoted.push(ch);
            }
        }
    }
    if backslashes > 0 {
        quoted.push_str(&"\\".repeat(backslashes * 2));
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_username_fits_windows_local_account_limit() {
        assert!(
            OFFLINE_USERNAME.chars().count() <= 20,
            "Windows local account names must be 20 characters or fewer"
        );
    }

    #[test]
    fn blocked_loopback_tcp_remote_ports_excludes_proxy_ports() {
        assert_eq!(
            blocked_loopback_tcp_remote_ports(&[39080, 39081]).as_deref(),
            Some("1-39079,39082-65535")
        );
    }

    #[test]
    fn setup_arg_parser_accepts_default_ports() {
        let parsed = parse_setup_args(&[]).expect("default setup args");
        assert_eq!(parsed.http_proxy_port, DEFAULT_HTTP_PROXY_PORT);
        assert_eq!(parsed.socks_proxy_port, DEFAULT_SOCKS_PROXY_PORT);
    }

    #[test]
    fn setup_arg_parser_rejects_same_ports() {
        let err = parse_setup_args(&[
            "--http-proxy-port".to_string(),
            "39080".to_string(),
            "--socks-proxy-port".to_string(),
            "39080".to_string(),
        ])
        .expect_err("same ports should fail");
        assert!(err.contains("must be different"));
    }
}
