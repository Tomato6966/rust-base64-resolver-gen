# Configuration

This application supports configuration through both configuration files and environment variables.

## Configuration File

Create a `config.toml` file in the project root with the following structure:

```toml
[server]
hostname = "0.0.0.0"
port = 3555

[database]
url = "postgresql://postgres:password@localhost:5432/image_db"
```

## Environment Variables

You can also override configuration using environment variables with the `APP_` prefix:

```bash
export APP_SERVER__HOSTNAME="0.0.0.0"
export APP_SERVER__PORT="3555"
export APP_DATABASE__URL="postgresql://postgres:password@localhost:5432/image_db"
```

## Configuration Priority

1. Environment variables (highest priority)
2. Configuration file (`config.toml`)

## Database URL Format

The database URL should follow the PostgreSQL connection string format:
```
postgresql://[user[:password]@][host][:port][/database][?param1=value1&...]
```

Example:
```
postgresql://postgres:password@localhost:5432/image_db
```
