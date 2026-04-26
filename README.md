# step-io-schema-check

EXPRESS schema 분석 도구 — entity 그룹 분류의 ground truth 를 schema 만으로 결정.

## Modes

```
cargo run --release -- catalog       # 한 번 실행: ENTITY_CATALOG.md + entity_catalog.json 생성
cargo run --release                  # default: check (placeholder, 미구현)
```

### `catalog` sub-command

`schemas/*.exp` 4개 STEP schema (AP203, AP203e2, AP214e3, AP242) 와 `groups.toml` 의 group 정의를 읽어 모든 entity 를 분류. **schema-only** — 외부 코드베이스 스캔 없음.

산출물:
- **ENTITY_CATALOG.md** — 인간 검토용. group 분포, 각 entity 의 분류 근거 (root supertype, schema 분포, confidence).
- **entity_catalog.json** — machine-readable. 미래 트레잇 마이그레이션 시 entity → group 매핑 import.

### Iterative refinement

1. `cargo run --release -- catalog` 실행 → catalog 생성
2. ENTITY_CATALOG.md 검토 (`_unclassified` 갯수, group 분포 균형, low-confidence 엔트리)
3. **`groups.toml` 갱신** — group 추가/제거/병합/패턴 보강. 코드 변경 X, recompile 불필요.
4. catalog 재실행 → 변경된 분류 결과
5. 분포 안정 (12~15 group 수렴) 까지 반복

### `check` mode (미구현)

step-io 의 트레잇 + per-module 리팩토링 도입 후 활성. trait introspection 으로 entity 의 NAME / ATTR_COUNT 추출 → schema 와 mismatch 검출.

## Schema 출처

`schemas/` 의 4 파일은 [STEPCode](https://github.com/stepcode/stepcode) 의 `data/` 에서 복사:

| schema | 출처 |
|---|---|
| ap203.exp | `stepcode/data/ap203/ap203.exp` |
| ap203e2_mim_lf.exp | `stepcode/data/ap203e2/ap203e2_mim_lf.exp` |
| ap214e3.exp | `stepcode/data/ap214e3/AP214E3_2010.exp` |
| ap242_mim_lf.exp | `stepcode/data/ap242/242_n8324_mim_lf.exp` |

step-io 의 mechanical CAD 도메인에 해당하는 4 schema 만 사용. AP209/AP210/AP238/AP239/AP240/IFC/ISO15926/pdm 등 다른 도메인 schema 는 제외.

Schema 갱신 시 (STEPCode 새 release):
```sh
cp ~/Desktop/references/stepcode/data/ap203/ap203.exp schemas/
cp ~/Desktop/references/stepcode/data/ap203e2/ap203e2_mim_lf.exp schemas/
cp ~/Desktop/references/stepcode/data/ap214e3/AP214E3_2010.exp schemas/ap214e3.exp
cp ~/Desktop/references/stepcode/data/ap242/242_n8324_mim_lf.exp schemas/ap242_mim_lf.exp
cargo run --release -- catalog       # 재실행
```

## Architecture

```
src/
├── main.rs           CLI dispatch (catalog | check)
├── express.rs        EXPRESS schema parser (.exp → EntitySchema)
├── inheritance.rs    SUBTYPE chain resolution (effective_attr_count, root_supertype, ancestors)
├── catalog.rs        groups.toml 읽고 자동 분류 + markdown / JSON 출력
└── check.rs          default mode placeholder (미래 trait introspection)
groups.toml           15 group 정의 (iterative refinement 결과)
schemas/              4 STEP schema (.exp 파일)
```

## Tests

```
cargo test
```

EXPRESS parser + inheritance resolution 의 단위 테스트 (CARTESIAN_POINT, SHAPE_ASPECT, ABSTRACT SUPERTYPE, DERIVE/WHERE 처리).
