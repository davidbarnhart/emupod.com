# The emupod server in a container (the second route after install.sh): docker build -t emupod . && docker run -p 8797:8797 -v emupod-cards:/cards emupod
# Build the wheel first (maturin build --release -o dist) and put it in ./dist, or let this file build it.
FROM rust:1-slim-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-pip python3-venv git && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN python3 -m venv /b && /b/bin/pip install -q maturin && /b/bin/maturin build --release -o /dist

FROM python:3.12-slim-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends git && rm -rf /var/lib/apt/lists/*
COPY --from=build /dist/*.whl /tmp/
RUN pip install --no-cache-dir /tmp/*.whl && rm /tmp/*.whl && useradd -m emupod && mkdir /cards && chown emupod /cards
USER emupod
VOLUME /cards
EXPOSE 8797
CMD ["emupod", "web", "--host", "0.0.0.0", "--port", "8797", "--cards", "/cards"]
