#![feature(can_vector)]
#![feature(write_all_vectored)]
#![feature(io_error_more)]
#![allow(clippy::try_err)]

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::io::{Result as IOResult, Error as IOError, ErrorKind as IOErrorKind, Write, Read};
use epoll_rs::{Epoll, Opts as PollOpts};
use serde::{Serialize, Deserialize};
use serde::{ser::Serialize as SerializeOwned,de::DeserializeOwned};
use stack_buffer::{StackBufReader};
use std::mem::MaybeUninit;
use arrayvec::ArrayVec;
use uuid::Uuid;

/// Used as the delimiter for HLAPI JSON packets
const DELIM: &[u8] = b"\0";

/// There's no practical limit when sending from Java to OC2 VMs
const READ_BUF: usize = 4096; // TODO: Benchmark different sizes trough file importing

/// Maximum size for sending buffers, limitation from OC2 VMs to Java, returns an empty error
const MAX_WRITE: usize = 4096; // TODO: try using buffers and benchmark

/// Main bus path
const MAIN_BUS: &str = "/dev/hvc0";

pub struct HLAPIBus {
    handle: File,
    poller: Epoll
}

pub type HLAPIDevice = Uuid;

#[derive(Serialize, Deserialize)]
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type", content = "data")]
pub enum HLAPISend {
    List,
    Methods (HLAPIDevice),
    Invoke {
        device_id: HLAPIDevice, // hyphenated
        #[serde(rename = "name")]
        method_name: String,
        parameters: Vec<!> // TODO: &dyn Serialize ?
    }
}

#[derive(Serialize, Deserialize)]
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "type", content = "data")]
pub enum HLAPIReceive {
    List (Vec<HLAPIDeviceDescriptor>),
    Methods (Vec<HLAPIMethod>),
    Error (Option<String>),
    Result (#[serde(default)] Vec<String>) // returned values
}

#[derive(Serialize, Deserialize)]
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HLAPIDeviceDescriptor {
    pub device_id: HLAPIDevice,
    #[serde(rename = "typeNames")]
    pub components: Vec<String> // cannot be empty thus no default
}

#[derive(Serialize, Deserialize)]
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HLAPIMethod {
    pub name: String,
    #[serde(default)] // skip_serializing_if = "Vec::is_empty")
    pub parameters: Vec<HLAPIType>, // Must get respected 1:1
    pub return_type: String, // always here ?

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_value_description: Option<String>
}

#[derive(Serialize, Deserialize)]
#[derive(Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HLAPIType {
    #[serde(rename = "type")]
    data: String
}

impl HLAPIBus {
    pub fn main_bus() -> IOResult<Self> {

        let poller = Epoll::new()?;
        let handle = poller.add(File::options().read(true).write(true).open(MAIN_BUS)?, PollOpts::IN)?.into_file();

        let descriptor = handle.as_raw_fd();
        let mut termios = termios::Termios::from_fd(descriptor)?;

        termios::cfmakeraw(&mut termios); // raw
        termios.c_lflag &= !termios::ECHO; // -echo
        termios::tcsetattr(descriptor, termios::TCSANOW, &termios)?; // immediate flush

        termios::cfsetspeed(&mut termios, termios::B38400)?; // baud 38400

        Ok(Self { handle, poller })
    }

    pub fn list(&mut self) -> IOResult<Vec<HLAPIDeviceDescriptor>> {
        self.write(&HLAPISend::List)?;
        let list: HLAPIReceive = self.read()?;
        if let HLAPIReceive::List(devices) = list {
            Ok(devices)
        } else { Err(IOErrorKind::InvalidData.into()) }
    }

    pub fn methods(&mut self, device: HLAPIDevice) -> IOResult<Vec<HLAPIMethod>> {
        self.write(&HLAPISend::Methods(device))?;
        let list: HLAPIReceive = self.read()?;
        if let HLAPIReceive::Methods(methods) = list {
            Ok(methods)
        } else { Err(IOErrorKind::InvalidData.into()) }
    }

    pub fn find(&mut self, name: &str) -> IOResult<HLAPIDevice> {
        for HLAPIDeviceDescriptor { device_id, components } in self.list()? {
            if components.into_iter().any(|dev| name == dev) { return Ok(device_id); }
        }
        Err(IOErrorKind::NotFound.into())
    }

    fn write<T: SerializeOwned>(&mut self, data: &T) -> IOResult<()> {
        let mut buffer = ArrayVec::<u8, MAX_WRITE>::new();

        buffer.write_all(DELIM)?;
        serde_json::to_writer(&mut buffer, data).map_err::<IOError, _>(|_| IOErrorKind::InvalidData.into())?;
        buffer.write_all(DELIM)?;

        self.handle.write_all(&buffer)?;
        self.handle.flush()?;

        Ok(())
    }

    /// Sends DELIM back into the socket, it makes so on the Java side, it clears the buffer, effectively resetting the state
    fn reset(&mut self) {
        self.handle.write_all(DELIM)?;
        self.handle.flush()?;
    }

    fn check_delim<R: Read>(buffer: &mut R) -> IOResult<()> {
        let mut delim_buf = [0; DELIM.len()];
        let bytes_read = buffer.read(&mut delim_buf)?;
        if bytes_read != DELIM.len() || delim_buf != DELIM {
            Err(IOErrorKind::UnexpectedEof)?
        } else { Ok(()) }
    }

    fn read<T: DeserializeOwned>(&mut self) -> IOResult<T> {
        self.poller.wait_one()?;
        let mut buffer = StackBufReader::<_, READ_BUF>::new(&mut self.handle);

        Self::check_delim(&mut buffer)?;

        let mut deserializer = serde_json::Deserializer::from_reader(&mut buffer);
        let data = T::deserialize(&mut deserializer)?;

        Self::check_delim(&mut buffer)?;

        Ok(data)
    }

}