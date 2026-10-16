# The host, as the release workflow builds it, in a distroless image. The
# binaries come prebuilt: dist/amd64/dcb-runtime-host and
# dist/arm64/dcb-runtime-host, from the release's Linux archives.
#
#   docker buildx build --platform linux/amd64,linux/arm64 .

# Where the compiled component is cached; owned by the user the host runs as.
FROM --platform=$BUILDPLATFORM busybox:1.37 AS cache
RUN mkdir /cache

FROM gcr.io/distroless/cc-debian12:nonroot
ARG TARGETARCH
COPY --chmod=755 dist/${TARGETARCH}/dcb-runtime-host /usr/local/bin/dcb-runtime-host
COPY --from=cache --chown=nonroot:nonroot /cache /cache
ENV DCB_LISTEN=0.0.0.0:8080 \
    DCB_WITH_DOCS=true \
    XDG_CACHE_HOME=/cache
EXPOSE 8080
ENTRYPOINT ["dcb-runtime-host"]
CMD ["serve"]
