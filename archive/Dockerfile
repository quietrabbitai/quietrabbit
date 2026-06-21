FROM python:3.12-slim

# Install system dependencies
# curl: healthcheck
# gcc + libsqlcipher-dev: sqlcipher3 build deps if binary wheel unavailable
RUN apt-get update && apt-get install -y \
    curl \
    gcc \
    libsqlcipher-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Install Python dependencies
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Verify SQLCipher is linked correctly — fails the build if not
RUN python -c "from sqlcipher3 import dbapi2 as sqlite3; conn = sqlite3.connect(':memory:'); conn.execute(\"PRAGMA key='test'\"); v = conn.execute('PRAGMA cipher_version').fetchone()[0]; print('SQLCipher OK: ' + v); conn.close()"

# Copy application code
COPY app/ ./app/
COPY conductor/ ./conductor/
COPY providers/ ./providers/
COPY auth/ ./auth/
COPY persistence/ ./persistence/
COPY taxonomy/ ./taxonomy/
COPY ui/ ./ui/
COPY scripts/ ./scripts/

# Generate taxonomy manifest at build time
RUN python scripts/generate_manifest.py

# Environment defaults
ENV QR_DATA_ROOT=/data/quietrabbit
ENV PYTHONUNBUFFERED=1

# Create data directory and non-root user
# Must happen before USER switch so ownership is correct
RUN useradd -m -u 2000 qr \
    && mkdir -p /data/quietrabbit \
    && chown -R qr:qr /data/quietrabbit

USER qr

HEALTHCHECK --interval=30s --timeout=10s --start-period=30s --retries=3 \
    CMD curl -f http://localhost:3000/health || exit 1

EXPOSE 3000

# Layer 0: ui module provides /health endpoint
# Will evolve to full Conductor entrypoint as layers are built
CMD ["python", "-m", "ui"]
