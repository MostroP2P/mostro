# StartOS wrapper for Mostro
# This Dockerfile pulls the mostro image from Docker Hub and configures it for StartOS

FROM arkanoider/mostro:latest

# Switch to root temporarily to create directories and set permissions
USER root

# Create data and config directories for StartOS
RUN mkdir -p /data /config && \
    chown mostrouser:mostrouser /data /config && \
    apk add --no-cache jq

# Copy the entrypoint script
COPY scripts/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh && chown mostrouser:mostrouser /entrypoint.sh

# Switch back to non-root user
USER mostrouser

# Set the working directory to /data for StartOS
WORKDIR /data

# Expose the RPC port
EXPOSE 8000

# Use the entrypoint script
ENTRYPOINT ["/entrypoint.sh"] 