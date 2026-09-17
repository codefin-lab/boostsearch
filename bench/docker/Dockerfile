FROM debian:bookworm-slim
COPY velosearch /usr/local/bin/velosearch
ENV VELOSEARCH_ADDR=0.0.0.0:9200
EXPOSE 9200
CMD ["/usr/local/bin/velosearch"]
