/* SPDX-License-Identifier: MIT */

use std::{
    env,
    fmt::Debug,
    fs::OpenOptions,
    io::{self, Read},
    path::Path,
};

use apple_nvram::{nvram_parse, VarType, Variable};

use ini::Ini;

#[derive(Debug)]
#[allow(dead_code)]
enum Error {
    Parse,
    SectionTooBig,
    ApplyError(std::io::Error),
    VariableNotFound,
    FileIO,
    IWDConfigDirNotFound,
}

impl From<apple_nvram::Error> for Error {
    fn from(e: apple_nvram::Error) -> Self {
        match e {
            apple_nvram::Error::ParseError => Error::Parse,
            apple_nvram::Error::SectionTooBig => Error::SectionTooBig,
            apple_nvram::Error::ApplyError(e) => Error::ApplyError(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(_e: io::Error) -> Self {
        Error::FileIO
    }
}

type Result<T> = std::result::Result<T, Error>;

fn main() {
    real_main().unwrap();
}

fn real_main() -> Result<()> {
    let matches = clap::command!()
        .arg(clap::arg!(-d --device [DEVICE] "Path to the nvram device."))
        .subcommand(clap::Command::new("list").about("Parse shared wlan keys from nvram"))
        .subcommand(
            clap::Command::new("sync")
                .about("Sync wlan information from nvram")
                .arg(clap::arg!(-c --config [CONFIG] "IWD config path.")),
        )
        .get_matches();

    let default_name = "/dev/mtd/by-name/nvram".to_owned();
    let default_config = "/var/lib/iwd".to_owned();
    let wlan_var = "preferred-networks";

    let mut file = OpenOptions::new()
        .read(true)
        .open(matches.get_one::<String>("device").unwrap_or(&default_name))
        .unwrap();
    let mut data = Vec::new();
    file.read_to_end(&mut data).unwrap();
    let mut nv = nvram_parse(&data)?;
    let active = nv.active_part_mut();
    let wlan_devs = active
        .get_variable(wlan_var.as_bytes(), VarType::System)
        .ok_or(Error::VariableNotFound)?;

    match matches.subcommand() {
        Some(("list", _args)) => {
            print_wlankeys(wlan_devs).expect("Failed to parse wlan device info");
        }
        Some(("sync", args)) => {
            sync_wlankeys(
                wlan_devs,
                args.get_one::<String>("config").unwrap_or(&default_config),
            )
            .expect("Failed to sync wlan device info");
        }
        _ => {
            print_wlankeys(wlan_devs).expect("Failed to parse wlan device info");
        }
    }
    Ok(())
}

struct Network {
    ssid: String,
    psk: Option<Vec<u8>>,
}

const CHUNK_LEN: usize = 0xc0;

fn parse_wlan_info(var: &dyn Variable) -> Vec<Network> {
    let mut nets = Vec::new();
    let data = var.value();
    for chunk in data.chunks(CHUNK_LEN) {
        let ssid_len = u32::from_le_bytes(chunk[0xc..0x10].try_into().unwrap()) as usize;
        let ssid = String::from_utf8_lossy(&chunk[0x10..0x10 + ssid_len]).to_string();
        let secure = u32::from_le_bytes(chunk[0x8..0xc].try_into().unwrap()) != 0;
        let psk = if secure {
            Some(chunk[0xa0..0xc0].to_owned())
        } else {
            None
        };
        nets.push(Network { ssid, psk });
    }

    nets
}

fn format_psk(psk: &[u8]) -> String {
    psk.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn print_wlankeys(var: &dyn Variable) -> Result<()> {
    let info = parse_wlan_info(var);

    for network in info {
        let psk_str = if let Some(psk) = network.psk {
            format!("PSK {}", format_psk(&psk))
        } else {
            "Open".to_owned()
        };
        println!("SSID {}, {}", network.ssid, psk_str);
    }
    Ok(())
}

fn sync_wlankeys(var: &dyn Variable, config: &String) -> Result<()> {
    let config_path = Path::new(config);

    if !config_path.is_dir() {
        return Err(Error::IWDConfigDirNotFound);
    }
    let nets = parse_wlan_info(var);

    for net in nets {
        let suffix = if net.psk.is_some() { ".psk" } else { ".open" };
        let net_path = config_path.join(format!("{}{}", net.ssid, suffix));

        if net_path.exists() {
            continue;
        }

        let mut info = Ini::new();
        if let Some(psk) = net.psk {
            info.with_section(Some("Security"))
                .set("PreSharedKey", format_psk(&psk));
        }
        info.write_to_file(net_path)?;
    }
    Ok(())
}
