# hiplay による改変

[symphonia-format-isomp4 0.5.5](https://crates.io/crates/symphonia-format-isomp4)（MPL-2.0）のコピー。
ワークスペースの `[patch.crates-io]` でこちらを使う。

## 変更点

- `src/atoms/ilst.rs` の `add_id3v1_genre_tag`
  - `gnre` が数値ではなく UTF-8 文字列で入っている m4a（mora で購入したファイルなど）で、
    2 バイトの数値として読もうとして `buffer underrun` になり、ファイル全体を開けなかった。
  - 文字列ならジャンル名として扱い、2 バイトに満たない値は無視するようにした。

上流で直ったら、このディレクトリと `[patch.crates-io]` を消して元のクレートに戻す。
