#!/bin/bash

echo "Starting local replica"
dfx start --clean --background

echo "Starting docker environment"
docker compose -f docker-compose.yml -f docker-compose-local.yml --env-file .env.local up -d --build
