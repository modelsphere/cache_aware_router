# 多阶段:builder = 预烤依赖的 base(cache_aware_router-builder,含 rust + gcc + libssl-dev + ca-cert +
# rsproxy cargo source),只跑 cargo build → CI 里【完全不碰 apt】,绕开某些 buildx runner 上 apt 验签被
# 篡改(NO_PUBKEY)+ 大包解压 lzma OOM 的坑(工具链已在干净网络机器上一次性 apt 装好烤进 base)。
#
# ⚠️ base 用 debian:11(bullseye,glibc 2.31):这些 buildx runner 的旧 seccomp profile 挡 clone3 系统调用,
#    glibc≥2.34(bookworm)的 pthread_create 走 clone3 → 建线程 EPERM(cargo/tokio 炸);glibc<2.34 走 clone → 放行。
#    代价:bullseye 是 openssl 1.1,故 runtime 需 COPY libssl.so.1.1(见下)。runner 修好 seccomp 后可换回 bookworm base。
#
# ⚠️ base 更新(rust 版本/系统依赖变)时在网络干净的机器(如 k8s-cpu-20)上重造并 push:
#   FROM docker.m.daocloud.io/library/debian:bullseye-slim
#   RUN apt-get update && apt-get install -y --no-install-recommends curl gcc g++ pkg-config libssl-dev ca-certificates
#   + rustup(RUSTUP_DIST_SERVER=https://rsproxy.cn)+ cargo source=rsproxy → docker build -t .../cache_aware_router-builder:<tag> && push

# ---------- builder ----------
FROM harbor.4pd.io/hardcore-tech/cache_aware_router-builder:1.88-bullseye AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# ---------- runtime ----------
FROM harbor.4pd.io/sagegpt-aio/pk_platform/ubuntu:24.04
# 不用 apt。从 builder COPY:① ca-cert(该 ubuntu base 无,CART reqwest 走 HTTPS 需要);
# ② openssl 1.1 的 .so(bullseye 编的二进制链 libssl.so.1.1,ubuntu:24.04 只有 .so.3)。
COPY --from=builder /etc/ssl/certs /etc/ssl/certs
COPY --from=builder /usr/lib/x86_64-linux-gnu/libssl.so.1.1 /usr/lib/x86_64-linux-gnu/
COPY --from=builder /usr/lib/x86_64-linux-gnu/libcrypto.so.1.1 /usr/lib/x86_64-linux-gnu/
RUN mkdir -p /workspace
WORKDIR /workspace
COPY --from=builder /src/target/release/cache-aware-router /workspace/cache-aware-router
COPY config.example.yaml /workspace/config.example.yaml
COPY launch_service /workspace/launch_service
RUN chmod +x /workspace/launch_service /workspace/cache-aware-router
ENTRYPOINT ["./launch_service"]
