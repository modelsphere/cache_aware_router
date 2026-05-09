FROM harbor.4pd.io/sagegpt-aio/pk_platform/ubuntu:24.04

RUN apt-get update && apt-get install -y vim-tiny ca-certificates
RUN rm -rf /var/lib/apt/lists/*

RUN mkdir /workspace
WORKDIR /workspace

COPY ./target/release/cache-aware-router /workspace/cache-aware-router
COPY ./config.example.yaml /workspace/config.example.yaml
COPY ./launch_service /workspace/launch_service

ENTRYPOINT ["./launch_service"]
