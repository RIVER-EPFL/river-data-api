# Two stages against one pinned OS. The compiler, the headers and renv exist only in the build
# stage; the runtime stage installs OpenCPU and receives the compiled packages as files, so the
# image that serves requests has no way to build or fetch anything.
#
# Both stages resolve r-base-core from the same apt snapshot in the same build, which is what keeps
# the packages compiled in the first stage ABI-compatible with the R that runs them in the second.
FROM ubuntu:26.04@sha256:678c6550cc43645e08669028bc177f50be4e7c5b8cca677067b1914d4afc7a03 AS build

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends software-properties-common ca-certificates \
    && add-apt-repository -y ppa:opencpu/opencpu-2.2 \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        opencpu-server build-essential gfortran curl \
        libicu-dev libssl-dev libxml2-dev libcurl4-openssl-dev libblas-dev liblapack-dev \
    && rm -rf /var/lib/apt/lists/*

# renv.lock was resolved against this R. A different R would silently re-resolve nothing but would
# produce packages the runtime stage cannot load, so the mismatch is caught here instead.
RUN R -e "stopifnot(getRversion() == '4.6.0')"

# renv is pinned too: it is what reads the lockfile, so an incompatible reader is the same
# reproducibility hole as an unpinned package.
RUN R -e "install.packages('renv', repos = 'https://cloud.r-project.org')" \
    && R -e "stopifnot(packageVersion('renv') == '1.2.4')"

COPY renv.lock /tmp/renv.lock
RUN R -e "renv::restore(lockfile = '/tmp/renv.lock', library = '/usr/local/lib/R/site-library', prompt = FALSE)" \
    && R -e "stopifnot(sapply(c('dplyr','tidyr','magrittr','pracma','signal','bigleaf'), requireNamespace, quietly = TRUE))"

COPY riverdata.tools /tmp/riverdata.tools
RUN R -e "install.packages('/tmp/riverdata.tools', lib = '/usr/local/lib/R/site-library', repos = NULL, type = 'source')" \
    && R -e "stopifnot(requireNamespace('riverdata.tools', quietly = TRUE))"


FROM ubuntu:26.04@sha256:678c6550cc43645e08669028bc177f50be4e7c5b8cca677067b1914d4afc7a03

ENV DEBIAN_FRONTEND=noninteractive

# curl is the healthcheck's only dependency; the base image ships neither it nor a shell with a
# built-in HTTP client, so a healthcheck written against it reported the container permanently
# unhealthy while it served normally. The certificates come first because the PPA is fetched over
# https.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*

# The PPA definition carries its own signing key inline, so it is copied rather than re-added,
# which keeps software-properties-common (and the Python it drags in) out of this stage.
COPY --from=build /etc/apt/sources.list.d/opencpu-ubuntu-opencpu-2_2-resolute.sources /etc/apt/sources.list.d/

# libapparmor1 is what OpenCPU's `unix` package links against, and that package is the fork with
# rlimits every request runs in, so without it the app does not start at all. It arrives with the
# apparmor recommends elsewhere; here recommends are off, so it is named.
#
# arrow is an OpenCPU output format nothing here requests, and at 380MB it is a fifth of the
# image; removing it in the installing layer is what makes the removal count against the size.
RUN apt-get update \
    && apt-get install -y --no-install-recommends opencpu-server libapparmor1 \
    && rm -rf /usr/lib/opencpu/library/arrow /usr/lib/R/library/arrow \
    && rm -rf /var/lib/apt/lists/* /usr/share/doc /usr/share/man /usr/share/info

COPY --from=build /usr/local/lib/R/site-library /usr/local/lib/R/site-library

# The account that serves requests. It owns nothing in the image, so every path outside the
# tmpfs mounts is readable and none of it is writable.
RUN groupadd --gid 10001 runner \
    && useradd --uid 10001 --gid 10001 --home-dir /tmp --shell /usr/sbin/nologin runner

# Everything OpenCPU's Debian packaging turns on that this runner does not serve: the default
# vhost, the TLS vhost (whose snakeoil key does not exist, so it aborts a non-root start anyway),
# the RStudio proxy, CGI, and the vhost access log that only root can open.
RUN a2dissite 000-default default-ssl rstudio \
    && a2disconf other-vhosts-access-log serve-cgi-bin \
    && a2dismod -f ssl proxy_http proxy status autoindex negotiation cgi \
    && : > /etc/apache2/ports.conf

# The packaged site wraps the handler in a VirtualHost on a fixed port. The port is decided at
# start instead (see entrypoint.sh), so the handler is declared against the main server.
COPY apache/opencpu.conf /etc/apache2/sites-available/opencpu.conf
COPY apache/runner.conf /etc/apache2/conf-available/runner.conf
RUN a2enconf runner

# Per-request limits, applied by OpenCPU to each forked worker. timelimit.post is below the API's
# own client timeout (TOOLS_RUNNER_TIMEOUT_SECONDS, 60s) so a runaway script comes back as a 400
# naming the timeout rather than as a client-side disconnect with the fork still running.
#
# The endpoints this runner serves are the ones under /ocpu/library; the app, user and session
# APIs are switched off.
COPY opencpu/riverdata.conf /etc/opencpu/server.conf.d/riverdata.conf

COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod 0755 /usr/local/bin/entrypoint.sh

# apt and dpkg are removed last: the image is complete, and a runtime with a package manager is a
# runtime that can install one more thing.
RUN rm -rf /usr/bin/apt /usr/bin/apt-get /usr/bin/apt-cache /usr/bin/apt-key /usr/bin/apt-config \
    /usr/bin/apt-mark /usr/bin/aptitude* /usr/bin/dpkg /usr/bin/dpkg-deb /usr/bin/dpkg-query \
    /usr/bin/dpkg-split /usr/bin/dpkg-trigger /usr/lib/apt /var/cache/apt

ARG IMAGE_BUILD_SHA=dev
ENV IMAGE_BUILD_SHA=$IMAGE_BUILD_SHA

# The listening port. 80 keeps the compose and Kubernetes service definitions unchanged; a runtime
# that does not lower net.ipv4.ip_unprivileged_port_start for containers sets this above 1024
# instead, since the server has no capability to bind a privileged port.
ENV RUNNER_PORT=80
EXPOSE 80

USER 10001:10001

# runtime_info exercises the whole path a tool call takes (Apache, the OpenCPU app, and a fork that
# loads riverdata.tools), so a 2xx here means the runner can actually run a script, which a static
# index page would not prove.
HEALTHCHECK --interval=10s --timeout=5s --start-period=30s --retries=12 \
    CMD curl -fsS -X POST -H 'Content-Type: application/json' -d '{}' \
        http://localhost:${RUNNER_PORT}/ocpu/library/riverdata.tools/R/runtime_info/json || exit 1

ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
