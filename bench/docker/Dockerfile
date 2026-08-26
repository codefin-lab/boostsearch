FROM debian:bookworm-slim
COPY obsearch /usr/local/bin/obsearch
ENV OBSEARCH_ADDR=0.0.0.0:9200
EXPOSE 9200
CMD ["/usr/local/bin/obsearch"]
