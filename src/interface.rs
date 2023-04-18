use std::io;

#[derive(Debug)]
pub enum Action<'a> {
    /// `Connect( ... ).0` - The given `u16` is the port of the original "client" socket. (`v_port`)
    ///
    /// When extracted:
    ///
    /// A socket has connected to the bridge.
    ///
    /// When inserted:
    ///
    /// A new connection should be emitted to the port of the Bridge.
    /// The `v_port` is to be remembered as it is used to identify following actions to their 
    /// respective connections.
    Connect(u16),
    /// `Data( ... ).0` - The `v_port`. Real port of the original "client" socket and id to the connection.
    /// `Data( ... ).1` - The binary `data` of this operation
    ///
    /// When extracted:
    /// 
    /// `data` has been extracted on for connection identified via `v_port`
    /// 
    /// When inserted:
    /// 
    /// `data` should be inserted to the connection indentified via `v_port`
    Data(u16, &'a [u8]),
    /// `Error( ... ).0` - The `v_port`. Real port of the original "client" socket and id to the connection.
    /// `Error( ... ).1` - The `error` 
    ///
    /// When extracted:
    /// 
    /// `error` has occured extracting data on for connection identified via `v_port`.
    /// This terminates this end of the connection.
    /// 
    /// When inserted:
    /// 
    /// The connection identified via `v_port` is no longer active and local socket should be terminated.
    Error(u16, io::Error),
    /// `AcceptError( ... ).0` - The `error`
    /// 
    /// When extracted (listening bridge only):
    /// 
    /// There has been a fatal `error` in accepting new connections to this Bridge.
    /// The Bridge will no longer accept connections and will terminate automatically as soon as the last
    /// active connection terminates.
    /// 
    /// When inserted (connectin bridge only):
    /// 
    /// Inserting following `Connect( ... )` actions should fail and the Bridge should terminate when all
    /// existing connections have terminated.
    AcceptError(io::Error),
}

#[derive(Debug)]
pub struct ErrorAction(pub u16, pub io::Error);
