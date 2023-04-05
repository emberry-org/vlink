use std::io;

#[derive(Debug)]
pub enum Action<'a> {
    /// "u16" is the v_port from which a new socket connected to the bridged port
    Connect(u16),
    /// "u16" is the v_port from which the data was sent into the bridged port
    Data(u16, &'a [u8]),
    /// "u16" is the v_port from which the socket threw and error
    Error(u16, io::Error),
    ///
    AcceptError(io::Error),
    /// NOP
    None,
}
