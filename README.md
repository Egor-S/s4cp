# s4cp: SQLite to S3 copy

A lightweight Go util and Docker image to back up SQLite databases to S3.

## Usage

```shell
export ACCESS_KEY_ID=<access key>
export SECRET_ACCESS_KEY=<secret key>
s4cp --endpoint-url <endpoint> --region <region> --bucket <bucket> <path> <key> 
```

## Docker

```shell
docker run -v <volume>:/data --env-file <env file> ghcr.io/egor-s/s4cp:v2 /data/<path> <key>
```

## strftime

`key` supports strftime syntax.
For example, `s4cp /data/data.db backups/%Y-%m-%d.db` will substitute the current date.
