FROM ubuntu:24.04

ARG RUNNER_VERSION=2.321.0
ARG RUNNER_ARCH=x64

RUN apt-get update && apt-get install -y --no-install-recommends \
        curl \
        ca-certificates \
        git \
        jq \
        tar \
        libicu74 \
    && rm -rf /var/lib/apt/lists/*

RUN groupadd --gid 1000 runner \
    && useradd --uid 1000 --gid runner --create-home --shell /bin/bash runner

RUN mkdir -p /home/runner/actions-runner \
    && curl -fsSLo /tmp/actions-runner.tar.gz \
        "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${RUNNER_ARCH}-${RUNNER_VERSION}.tar.gz" \
    && tar xzf /tmp/actions-runner.tar.gz -C /home/runner/actions-runner \
    && rm /tmp/actions-runner.tar.gz \
    && /home/runner/actions-runner/bin/installdependencies.sh \
    && chown -R runner:runner /home/runner/actions-runner

COPY --chown=runner:runner entrypoint.sh /home/runner/entrypoint.sh
RUN chmod +x /home/runner/entrypoint.sh

# Non-root: the runner process (and any job it executes) never has root in the container.
USER runner
WORKDIR /home/runner/actions-runner

ENTRYPOINT ["/home/runner/entrypoint.sh"]
