# Throwaway runner for agent-generated code: stdlib Python + pandas, no network at runtime,
# non-root, read-only root fs (flags set by server/src/sandbox.rs).
FROM python:3.12-slim
RUN pip install --no-cache-dir pandas==2.3.3 && useradd -u 10001 -M -s /usr/sbin/nologin runner
WORKDIR /work
USER 10001
ENV PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 HOME=/tmp
ENTRYPOINT ["python3", "-I", "/code/main.py"]
