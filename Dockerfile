# 运行时镜像：无头精简系统（alpine）+ CI 预编译的 musl 静态二进制，全镜像约 20 MB。
#
# 二进制由 .github/workflows/docker.yml 用 cross 交叉编译后放进构建上下文
# （aequi-linux-amd64 / aequi-linux-arm64），因此这里不再编译——
# 本地想单独构建镜像，先按 rust_release.yml 的方式产出对应架构的二进制。
#
# TLS 用 rustls（webpki-roots 内置证书），SQLite 静态链接，
# 所以运行时不需要 ca-certificates 等任何额外包。
FROM alpine:3.21

# BuildKit 在多平台构建时自动注入目标架构（amd64 / arm64）。
ARG TARGETARCH=amd64

COPY aequi-linux-${TARGETARCH} /usr/local/bin/aequi

# 专用用户与数据目录：SQLite 需要对 /data 的写权限。
RUN chmod +x /usr/local/bin/aequi \
    && addgroup -S aequi \
    && adduser -S -G aequi -h /data aequi \
    && mkdir -p /data \
    && chown -R aequi:aequi /data

# 两个数据库都落在 /data，挂卷即持久化。
# 首次启动可通过环境变量初始化管理员账号（AEQUI_ADMIN_USERNAME / AEQUI_ADMIN_PASSWORD），
# 仅在数据库不存在时生效。
ENV AEQUI_DATABASE_PATH=/data/aequi.db \
    AEQUI_DATA_DATABASE_PATH=/data/aequi-data.db

VOLUME ["/data"]
EXPOSE 8080

USER aequi
WORKDIR /data

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=3 \
    CMD wget -qO- http://127.0.0.1:8080/health >/dev/null 2>&1 || exit 1

ENTRYPOINT ["/usr/local/bin/aequi"]
