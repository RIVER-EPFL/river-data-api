# tools-runner

**Executes analytical tool R scripts for the River Data API.**

Tool scripts live in the API's database; this image only runs them. It is
[OpenCPU](https://www.opencpu.org/) plus the science packages the portal calculations use
(dplyr, tidyr, pracma, signal, bigleaf) and one small package, `riverdata.tools`.

## Usage

```bash
curl -s http://localhost:8006/ocpu/library/riverdata.tools/R/run_tool/json \
  -H 'Content-Type: application/json' \
  -d '{"script": "tool <- function(inputs, constants, curves) list(x = inputs$a + inputs$b)",
       "entry": "tool", "inputs": {"a": 1, "b": 2}}'
```

Returns `{"x": [3]}` (OpenCPU serialises R scalars as one-element arrays). Script errors come
back as HTTP 400 with a JSON first line carrying the message, call and traceback.

The other endpoints, same `POST .../R/<fn>/json` shape:

- `runtime_info` reports the R version, package versions and image build.
- `inspect_script` reads the inputs, constants, curves and output keys off a script's parse
  tree, for proposing a manifest. Nothing is evaluated. Names built at runtime are flagged
  (`dynamic_outputs`, `dynamic_reads`) rather than guessed.
- `parse_check` is the syntax check alone; a syntax error is a normal result, not a 400.
- `scan_script` reports call structure with line numbers, for the API's safety lint. Which
  names are refused is the API's list; this only reports structure.

## Build and run

```bash
docker build -t river-data-tools-r .
docker run -p 8006:80 --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  --tmpfs /tmp:uid=10001,gid=10001,mode=1777,size=256m \
  --tmpfs /run/apache2:uid=10001,gid=10001,mode=0750 \
  --tmpfs /var/lock/apache2:uid=10001,gid=10001,mode=0750 \
  --tmpfs /var/log/apache2:uid=10001,gid=10001,mode=0750 \
  --tmpfs /var/log/opencpu:uid=10001,gid=10001,mode=0750 \
  river-data-tools-r
```

Those are the flags the `river-data-ui` compose sets. The same run with `--network none` serves
every endpoint, proving the image needs nothing from outside itself.

`RUNNER_PORT` sets the listen port (default 80). Set it above 1024 on runtimes that keep
privileged ports privileged inside containers. `RUNNER_ADDRESS` restricts the listener to one
address; the Kubernetes deployment sets `127.0.0.1` so the runner, a second container in the API
pod, is reachable only from inside that pod.

## Tests

The package carries a testthat suite under `riverdata.tools/tests`. The image build runs it in
the build stage against the installed package, so `docker build` is the test run: a failing test
produces no image. To run it on a host R with testthat installed:

```bash
cd riverdata.tools
Rscript -e 'library(testthat); for (f in list.files("R", full.names = TRUE)) source(f); test_dir("tests/testthat", load_package = "none")'
```

## Pinning

`renv.lock` records every package version, testthat included, resolved against R 4.6.0; both build stages assert
that R version and the base image is pinned by digest. To re-resolve, install the new versions
in a running container and `renv::snapshot()` back over the lockfile.

## Security model

The container is the boundary: a tool script is arbitrary R, so what limits it is what the
container can reach. It runs as uid 10001 with no capabilities, a read-only filesystem, no
compiler, no package manager, no credentials and no database access. Each request runs in a
fork with its own memory, file-size and process limits and a 30 second timeout. Everything a
script needs (inputs, resolved constants, curve coefficients) arrives in the request body.
Deny egress in deployments; the API-side lint is accident protection for authors, not the
control.
