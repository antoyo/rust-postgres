#[link(name = "postgres",
       vers = "0.1",
       url = "https://github.com/sfackler/rust-postgres")];

extern mod extra;

use extra::digest::Digest;
use extra::md5::Md5;
use extra::url::{UserInfo, Url};
use std::cell::Cell;
use std::hashmap::HashMap;
use std::rt::io::{io_error, Decorator};
use std::rt::io::mem::MemWriter;
use std::rt::io::net::ip::SocketAddr;
use std::rt::io::net::tcp::TcpStream;

use message::*;
use types::{Oid, ToSql, FromSql};

mod message;
mod types;

macro_rules! match_read_message(
    ($conn:expr, { $($($p:pat)|+ => $e:expr),+ }) => (
        match {
            let ref conn = $conn;
            let resp;
            loop {
                match conn.read_message() {
                    NoticeResponse { fields } => handle_notice_response(fields),
                    msg => {
                        resp = msg;
                        break;
                    }
                }
            }
            resp
        } {
            $(
                $($p)|+ => $e
            ),+
        }
    )
)

macro_rules! match_read_message_or_fail(
    ($conn:expr, { $($($p:pat)|+ => $e:expr),+ }) => (
        match_read_message!($conn, {
            $(
              $($p)|+ => $e
            ),+ ,
            resp => fail2!("Bad response: {}", resp.to_str())
        })
    )
)

fn handle_notice_response(fields: ~[(u8, ~str)]) {
    let err = PostgresDbError::new(fields);
    info2!("{}: {}", err.severity, err.message);
}

#[deriving(ToStr)]
pub enum PostgresConnectError {
    InvalidUrl,
    MissingUser,
    DbError(PostgresDbError),
    MissingPassword,
    UnsupportedAuthentication
}

#[deriving(ToStr)]
pub enum PostgresErrorPosition {
    Position(uint),
    InternalPosition {
        position: uint,
        query: ~str
    }
}

#[deriving(ToStr)]
pub struct PostgresDbError {
    // This could almost be an enum, except the values can be localized :(
    severity: ~str,
    // Should probably end up as an enum
    code: ~str,
    message: ~str,
    position: Option<PostgresErrorPosition>,
    where: Option<~str>,
    file: ~str,
    line: uint,
    routine: ~str
}

impl PostgresDbError {
    fn new(fields: ~[(u8, ~str)]) -> PostgresDbError {
        // move_rev_iter is more efficient than move_iter
        let mut map: HashMap<u8, ~str> = fields.move_rev_iter().collect();
        PostgresDbError {
            severity: map.pop(&('S' as u8)).unwrap(),
            code: map.pop(&('C' as u8)).unwrap(),
            message: map.pop(&('M' as u8)).unwrap(),
            position: match map.pop(&('P' as u8)) {
                Some(pos) => Some(Position(FromStr::from_str(pos).unwrap())),
                None => match map.pop(&('p' as u8)) {
                    Some(pos) => Some(InternalPosition {
                        position: FromStr::from_str(pos).unwrap(),
                        query: map.pop(&('q' as u8)).unwrap()
                    }),
                    None => None
                }
            },
            where: map.pop(&('W' as u8)),
            file: map.pop(&('F' as u8)).unwrap(),
            line: FromStr::from_str(map.pop(&('L' as u8)).unwrap()).unwrap(),
            routine: map.pop(&('R' as u8)).unwrap()
        }
    }
}

pub struct PostgresConnection {
    priv stream: Cell<TcpStream>,
    priv next_stmt_id: Cell<int>
}

impl Drop for PostgresConnection {
    fn drop(&self) {
        do io_error::cond.trap(|_| {}).inside {
            self.write_message(&Terminate);
        }
    }
}

impl PostgresConnection {
    pub fn connect(url: &str) -> PostgresConnection {
        match PostgresConnection::try_connect(url) {
            Ok(conn) => conn,
            Err(err) => fail2!("Failed to connect: {}", err.to_str())
        }
    }

    pub fn try_connect(url: &str) -> Result<PostgresConnection,
                                            PostgresConnectError> {
        let Url {
            host,
            port,
            user,
            path,
            query: args,
            _
        }: Url = match FromStr::from_str(url) {
            Some(url) => url,
            None => return Err(InvalidUrl)
        };
        let user = match user {
            Some(user) => user,
            None => return Err(MissingUser)
        };
        let mut args = args;

        // This seems silly
        let socket_url = format!("{}:{}", host,
                                 port.unwrap_or_default(~"5432"));
        let addr: SocketAddr = match FromStr::from_str(socket_url) {
            Some(addr) => addr,
            None => return Err(InvalidUrl)
        };

        let conn = PostgresConnection {
            // Need to figure out what to do about unwrap here
            stream: Cell::new(TcpStream::connect(addr).unwrap()),
            next_stmt_id: Cell::new(0)
        };

        // We have to clone here since we need the user again for auth
        args.push((~"user", user.user.clone()));
        if !path.is_empty() {
            args.push((~"database", path));
        }
        conn.write_message(&StartupMessage {
            version: PROTOCOL_VERSION,
            parameters: args.as_slice()
        });

        match conn.handle_auth(user) {
            Some(err) => return Err(err),
            None => ()
        }

        loop {
            match_read_message_or_fail!(conn, {
                ParameterStatus { parameter, value } =>
                    info!("Parameter %s = %s", parameter, value),
                BackendKeyData {_} => (),
                ReadyForQuery {_} => break
            })
        }

        Ok(conn)
    }

    fn write_messages(&self, messages: &[&FrontendMessage]) {
        let mut buf = MemWriter::new();
        for &message in messages.iter() {
            buf.write_message(message);
        }
        do self.stream.with_mut_ref |s| {
            s.write(buf.inner_ref().as_slice());
        }
    }

    fn write_message(&self, message: &FrontendMessage) {
        do self.stream.with_mut_ref |s| {
            s.write_message(message);
        }
    }

    fn read_message(&self) -> BackendMessage {
        do self.stream.with_mut_ref |s| {
            s.read_message()
        }
    }

    fn handle_auth(&self, user: UserInfo) -> Option<PostgresConnectError> {
        match_read_message_or_fail!(self, {
            AuthenticationOk => return None,
            AuthenticationCleartextPassword => {
                let pass = match user.pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                self.write_message(&PasswordMessage { password: pass });
            },
            AuthenticationMD5Password { salt } => {
                let UserInfo { user, pass } = user;
                let pass = match pass {
                    Some(pass) => pass,
                    None => return Some(MissingPassword)
                };
                let input = pass + user;
                let mut md5 = Md5::new();
                md5.input_str(input);
                let output = md5.result_str();
                md5.reset();
                md5.input_str(output);
                md5.input(salt);
                let output = "md5" + md5.result_str();
                self.write_message(&PasswordMessage {
                    password: output.as_slice()
                });
            }
        })

        match_read_message_or_fail!(self, {
            AuthenticationOk => None,
            ErrorResponse { fields } =>
                Some(DbError(PostgresDbError::new(fields)))
        })
    }

    pub fn prepare<'a>(&'a self, query: &str) -> NormalPostgresStatement<'a> {
        match self.try_prepare(query) {
            Ok(stmt) => stmt,
            Err(err) => fail2!("Error preparing \"{}\": {}", query,
                               err.to_str())
        }
    }

    pub fn try_prepare<'a>(&'a self, query: &str)
                -> Result<NormalPostgresStatement<'a>, PostgresDbError> {
        let id = self.next_stmt_id.take();
        let stmt_name = format!("statement_{}", id);
        self.next_stmt_id.put_back(id + 1);

        let types = [];
        self.write_messages([
            &Parse {
                name: stmt_name,
                query: query,
                param_types: types
            },
            &Describe {
                variant: 'S' as u8,
                name: stmt_name
            },
            &Sync]);

        match_read_message_or_fail!(self, {
            ParseComplete => (),
            ErrorResponse { fields } => {
                self.wait_for_ready();
                return Err(PostgresDbError::new(fields));
            }
        })

        let param_types = match_read_message_or_fail!(self, {
            ParameterDescription { types } => types
        });

        let result_desc = match_read_message_or_fail!(self, {
            RowDescription { descriptions } => descriptions,
            NoData => ~[]
        });

        self.wait_for_ready();

        Ok(NormalPostgresStatement {
            conn: self,
            name: stmt_name,
            param_types: param_types,
            result_desc: result_desc,
        })
    }

    pub fn in_transaction<T>(&self, blk: &fn(&PostgresTransaction) -> T) -> T {
        self.quick_query("BEGIN");

        let trans = PostgresTransaction {
            conn: self,
            commit: Cell::new(true)
        };
        // If this fails, Postgres will rollback when the connection closes
        let ret = blk(&trans);

        if trans.commit.take() {
            self.quick_query("COMMIT");
        } else {
            self.quick_query("ROLLBACK");
        }

        ret
    }

    pub fn update(&self, query: &str, params: &[&ToSql]) -> uint {
        match self.try_update(query, params) {
            Ok(res) => res,
            Err(err) => fail2!("Error running update: {}", err.to_str())
        }
    }

    pub fn try_update(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        do self.try_prepare(query).chain |stmt| {
            stmt.try_update(params)
        }
    }

    fn quick_query(&self, query: &str) {
        self.write_message(&Query { query: query });

        loop {
            match_read_message!(self, {
                ReadyForQuery {_} => break,
                ErrorResponse { fields } =>
                    fail2!("Error: {}", PostgresDbError::new(fields).to_str()),
                _ => ()
            })
        }
    }

    fn wait_for_ready(&self) {
        match_read_message_or_fail!(self, {
            ReadyForQuery {_} => ()
        })
    }
}

pub struct PostgresTransaction<'self> {
    priv conn: &'self PostgresConnection,
    priv commit: Cell<bool>
}

impl<'self> PostgresTransaction<'self> {
    pub fn prepare<'a>(&'a self, query: &str)
            -> TransactionalPostgresStatement<'a> {
        TransactionalPostgresStatement { stmt: self.conn.prepare(query) }
    }

    pub fn try_prepare<'a>(&'a self, query: &str)
            -> Result<TransactionalPostgresStatement<'a>, PostgresDbError> {
        do self.conn.try_prepare(query).map_move |stmt| {
            TransactionalPostgresStatement { stmt: stmt }
        }
    }

    pub fn update(&self, query: &str, params: &[&ToSql]) -> uint {
        self.conn.update(query, params)
    }

    pub fn try_update(&self, query: &str, params: &[&ToSql])
            -> Result<uint, PostgresDbError> {
        self.conn.try_update(query, params)
    }

    pub fn will_commit(&self) -> bool {
        let commit = self.commit.take();
        self.commit.put_back(commit);
        commit
    }

    pub fn set_commit(&self) {
        self.commit.take();
        self.commit.put_back(true);
    }

    pub fn set_rollback(&self) {
        self.commit.take();
        self.commit.put_back(false);
    }
}

pub trait PostgresStatement {
    fn num_params(&self) -> uint;
    fn update(&self, params: &[&ToSql]) -> uint;
    fn try_update(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError>;
    fn query<'a>(&'a self, params: &[&ToSql]) -> PostgresResult<'a>;
    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError>;
    fn find_col_named(&self, col: &str) -> Option<uint>;
}

pub struct NormalPostgresStatement<'self> {
    priv conn: &'self PostgresConnection,
    priv name: ~str,
    priv param_types: ~[Oid],
    priv result_desc: ~[RowDescriptionEntry],
}

#[unsafe_destructor]
impl<'self> Drop for NormalPostgresStatement<'self> {
    fn drop(&self) {
        do io_error::cond.trap(|_| {}).inside {
            self.conn.write_messages([
                &Close {
                    variant: 'S' as u8,
                    name: self.name.as_slice()
                },
                &Sync]);
            loop {
                match_read_message!(self.conn, {
                    ReadyForQuery {_} => break,
                    _ => ()
                })
            }
        }
    }
}

impl<'self> NormalPostgresStatement<'self> {
    fn execute(&self, portal_name: &str, params: &[&ToSql])
            -> Option<PostgresDbError> {
        let mut formats = ~[];
        let mut values = ~[];
        for (&param, &ty) in params.iter().zip(self.param_types.iter()) {
            let (format, value) = param.to_sql(ty);
            formats.push(format as i16);
            values.push(value);
        };

        let result_formats: ~[i16] = self.result_desc.iter().map(|desc| {
            types::result_format(desc.type_oid) as i16
        }).collect();

        self.conn.write_messages([
            &Bind {
                portal: portal_name,
                statement: self.name.as_slice(),
                formats: formats,
                values: values,
                result_formats: result_formats
            },
            &Execute {
                portal: portal_name,
                max_rows: 0
            },
            &Sync]);

        match_read_message_or_fail!(self.conn, {
            BindComplete => None,
            ErrorResponse { fields } => {
                self.conn.wait_for_ready();
                Some(PostgresDbError::new(fields))
            }
        })
    }

    fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<'a> {
        match self.try_lazy_query(row_limit, params) {
            Ok(result) => result,
            Err(err) => fail2!("Error executing query: {}", err.to_str())
        }
    }

    fn try_lazy_query<'a>(&'a self, _row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        match self.execute("", params) {
            Some(err) => {
                return Err(err);
            }
            None => ()
        }

        let mut data = ~[];
        loop {
            match_read_message_or_fail!(self.conn, {
                EmptyQueryResponse |
                CommandComplete {_} => {
                    break;
                },
                DataRow { row } => data.push(row)
            })
        }
        self.conn.wait_for_ready();

        // we're going to be popping off
        data.reverse();
        Ok(PostgresResult {
            stmt: self,
            data: data,
        })
    }
}

impl<'self> PostgresStatement for NormalPostgresStatement<'self> {
    fn num_params(&self) -> uint {
        self.param_types.len()
    }

    fn update(&self, params: &[&ToSql]) -> uint {
        match self.try_update(params) {
            Ok(count) => count,
            Err(err) => fail2!("Error running update: {}", err.to_str())
        }
    }

    fn try_update(&self, params: &[&ToSql])
                      -> Result<uint, PostgresDbError> {
        match self.execute("", params) {
            Some(err) => {
                return Err(err);
            }
            None => ()
        }

        let num;
        loop {
            match_read_message_or_fail!(self.conn, {
                CommandComplete { tag } => {
                    let s = tag.split_iter(' ').last().unwrap();
                    num = match FromStr::from_str(s) {
                        None => 0,
                        Some(n) => n
                    };
                    break;
                },
                DataRow {_} => (),
                EmptyQueryResponse => {
                    num = 0;
                    break;
                },
                NoticeResponse {_} => (),
                ErrorResponse { fields } => {
                    self.conn.wait_for_ready();
                    return Err(PostgresDbError::new(fields));
                }
            })
        }
        self.conn.wait_for_ready();

        Ok(num)
    }

    fn query<'a>(&'a self, params: &[&ToSql])
            -> PostgresResult<'a> {
        self.lazy_query(0, params)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.try_lazy_query(0, params)
    }

    fn find_col_named(&self, col: &str) -> Option<uint> {
        do self.result_desc.iter().position |desc| {
            desc.name.as_slice() == col
        }
    }
}

pub struct TransactionalPostgresStatement<'self> {
    priv stmt: NormalPostgresStatement<'self>
}

impl<'self> PostgresStatement for TransactionalPostgresStatement<'self> {
    fn num_params(&self) -> uint {
        self.stmt.num_params()
    }

    fn update(&self, params: &[&ToSql]) -> uint {
        self.stmt.update(params)
    }

    fn try_update(&self, params: &[&ToSql]) -> Result<uint, PostgresDbError> {
        self.stmt.try_update(params)
    }

    fn query<'a>(&'a self, params: &[&ToSql]) -> PostgresResult<'a> {
        self.stmt.query(params)
    }

    fn try_query<'a>(&'a self, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.stmt.try_query(params)
    }

    fn find_col_named(&self, col: &str) -> Option<uint> {
        self.stmt.find_col_named(col)
    }
}

impl<'self> TransactionalPostgresStatement<'self> {
    pub fn lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> PostgresResult<'a> {
        self.stmt.lazy_query(row_limit, params)
    }

    pub fn try_lazy_query<'a>(&'a self, row_limit: uint, params: &[&ToSql])
            -> Result<PostgresResult<'a>, PostgresDbError> {
        self.try_lazy_query(row_limit, params)
    }
}

pub struct PostgresResult<'self> {
    priv stmt: &'self NormalPostgresStatement<'self>,
    priv data: ~[~[Option<~[u8]>]]
}

impl<'self> Iterator<PostgresRow<'self>> for PostgresResult<'self> {
    fn next(&mut self) -> Option<PostgresRow<'self>> {
        do self.data.pop_opt().map_move |row| {
            PostgresRow {
                stmt: self.stmt,
                data: row
            }
        }
    }
}

pub struct PostgresRow<'self> {
    priv stmt: &'self NormalPostgresStatement<'self>,
    priv data: ~[Option<~[u8]>]
}

impl<'self> Container for PostgresRow<'self> {
    fn len(&self) -> uint {
        self.data.len()
    }
}

impl<'self, I: RowIndex, T: FromSql> Index<I, T> for PostgresRow<'self> {
    fn index(&self, idx: &I) -> T {
        let idx = idx.idx(self.stmt);
        FromSql::from_sql(self.stmt.result_desc[idx].type_oid,
                          &self.data[idx])
    }
}

pub trait RowIndex {
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint;
}

impl RowIndex for uint {
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        *self
    }
}

// This is a convenicence as the 0 in get[0] resolves to int :(
impl RowIndex for int {
    fn idx(&self, _stmt: &NormalPostgresStatement) -> uint {
        assert!(*self >= 0);
        *self as uint
    }
}

impl<'self> RowIndex for &'self str {
    fn idx(&self, stmt: &NormalPostgresStatement) -> uint {
        match stmt.find_col_named(*self) {
            Some(idx) => idx,
            None => fail2!("No column with name {}", *self)
        }
    }
}
