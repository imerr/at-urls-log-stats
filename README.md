at-urls-log-stats
===
Prometheus exporter for requests per domain/status code extracted from docker logs of the urls project


### Running with docker compose

```yaml
  at-urls-log-stats:
      container_name: at-urls-log-stats
      image: ghcr.io/imerr/at-urls-log-stats:master
      volumes:
          - '/var/run/docker.sock:/var/run/docker.sock'
          - '/opt/at-urls-log-stats/config.json:/config.json'
      ports:
          - "8000:8000"
      restart: unless-stopped
```

