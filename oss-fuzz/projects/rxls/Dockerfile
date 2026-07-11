FROM gcr.io/oss-fuzz-base/base-builder-rust
RUN git clone --depth 1 https://github.com/HyunjoJung/rxls rxls
WORKDIR rxls
COPY build.sh $SRC/
