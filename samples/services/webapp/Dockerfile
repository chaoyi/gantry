FROM alpine:3.20
RUN apk add --no-cache socat
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
WORKDIR /app
CMD ["/entrypoint.sh"]
