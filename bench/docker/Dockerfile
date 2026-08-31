FROM debian:bookworm-slim
COPY boostsearch /usr/local/bin/boostsearch
ENV BOOSTSEARCH_ADDR=0.0.0.0:9200
EXPOSE 9200
CMD ["/usr/local/bin/boostsearch"]
