# pgsqlite

pgsqlite lets PostgreSQL clients query SQLite databases through the PostgreSQL wire protocol.  
The project is experimental and is not ready for production use.  

**Security:** The server accepts connections without password authentication and listens on all TCP interfaces by default.  
Use a trusted network or `--no-tcp` for local Unix-socket access.  

## Install and run

Download a binary from [Releases](https://github.com/ParkSnoopy/pgsqlite/releases), or build from source:

```bash
git clone https://github.com/ParkSnoopy/pgsqlite.git
cd pgsqlite
cargo build --release --bin pgsqlite
./target/release/pgsqlite --database mydb.db
```

Connect using a PostgreSQL client:

```bash
psql "host=localhost port=5432 dbname=mydb user=postgres"
```

Use `--in-memory` for an ephemeral database, `--ssl` for TLS, or `--help` for options.  
SQLite permits one writer at a time; PostgreSQL compatibility is incomplete.  

## Documentation

- [Configuration](docs/configuration.md), [TLS](docs/ssl-setup.md), and [Unix sockets](docs/unix-sockets.md)
- [Type mapping](docs/type-mapping-prd.md), [arrays](docs/array-support.md), and [JSON](docs/json-support.md)
- [Migrations](docs/migrations.md), [security](docs/security.md), and [performance](docs/performance.md)
- [Architecture](docs/architecture.md), [development and testing](docs/development.md), and [backlog](docs/backlog.md)

## License

The project is licensed under [GPL-3.0-only](LICENSE) after commit `00ee9ed5c3e88f26203281d378a274da7a4da6ee`.  
That commit and earlier versions are available under [Apache-2.0](LICENSE.ORIG); existing Apache-2.0 grants for that material remain in effect.  