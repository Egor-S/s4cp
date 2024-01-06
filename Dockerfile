FROM golang:1.21-alpine AS builder
WORKDIR /app
COPY . .
RUN apk add --update gcc musl-dev
RUN go build -o s4cp cmd/s4cp/main.go

FROM alpine:latest
WORKDIR /app
COPY --from=builder /app/s4cp .
ENTRYPOINT ["./s4cp"]
