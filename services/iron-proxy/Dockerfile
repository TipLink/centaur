FROM ironsh/iron-proxy:0.34.0@sha256:d5691525185f9cfe30244f36c888cde0eb5b1ce54dd5f6fd3ac76978b9fe5fef

USER root
RUN apk add --no-cache curl jq

COPY services/iron-proxy/iron-proxy.yaml /usr/local/share/iron-proxy/proxy.yaml.default
COPY --chmod=755 services/iron-proxy/entrypoint.sh /entrypoint.sh

EXPOSE 8080

ENTRYPOINT ["/entrypoint.sh"]
