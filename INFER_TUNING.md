# infer 파이프라인 — 미흡 부분 (정밀화 대상)

전체 3 stage end-to-end 동작은 확인됨 (commit `3cfa522` ~ `0ead7f2`).
첫 cut 분포는 다음과 같음:

| Stage | confident | review | unresolved | 입력 | 출력 |
|---|---|---|---|---|---|
| variant | 825 | 828 | 127 | 1,780 entity (4 schema union) | 1,780 결정 |
| arena | 362 | 0 | 0 | 825 confident variant | 362 group |
| pool | 362 | 0 | 0 | 362 arena | 94 pool (connected component) |

variant 단순화 후 (commit `ac2dbf8`) 의 분포:

| Stage | output | 비고 |
|---|---|---|
| variant | 1,780 (single=349 enum=1362 nested=69) | bucket/pending 폐기, 모두 자동 결정 |
| arena | 421 group | strict gate 자연 통과 |
| pool | (미측정) | — |

SUPERTYPE 절 파서 + 분류기 정밀화 후 분포:

| Stage | output | 비고 |
|---|---|---|
| variant | 1,780 (single=239 enum=1244 nested=12 enum_base=173 merged_into=97 complex=13 composite=2) | unresolved=0, parse_warnings=0 (4 schema 모두) |
| arena | 436 group | confident=436 review=0 unresolved=0 |
| pool | 436 group → 97 distinct pool | confident=436 review=0 unresolved=0 |

분포 변동의 본질: 이전 단순 분류기가 silent fallback / 누락된 mixin 으로
부정확하게 single / enum 으로 분류하던 entity 들이 정확한 자리로 이동
(enum_base / merged_into / complex_supertype / composite_one_of 가 명시
변형으로 분리됨).

본 문서는 정밀화 단계에서 다뤄야 할 미흡 부분을 항목별로 정리. 임시 문서로,
정밀화 후 폐기 또는 INFRA_PLAN.md 같은 영구 문서로 이관 예정.

## 1. SUPERTYPE 절 파싱 정밀도 — 처리됨

**상황**: 이전 regex + paren-depth 기반 ad-hoc 파서가 `ONEOF (...) AND
ONEOF (...)` 의 `AND` 키워드를 인식 못 했고, `(ref ANDOR ONEOF (...))` 의
leading bare ref 를 누락하고, `ONEOF (..., (loop ANDOR path))` 같은 ONEOF
멤버 안의 합성 노드를 평탄화로 silent 오분류했다. 4 unique entity 가
silent 깨짐 + 14 unique 패턴이 부정확.

**해결**: EXPRESS § 9.2.4 그대로의 recursive descent 파서 + 트리 기반
`SupertypeExpr` IR + `VariantSpec::ComplexSupertype` 의 mixin 필드를 raw
SupertypeExpr 로 + B7 자동 분류 (`VariantSpec::CompositeOneOf` + `Rule
1.5`) + Rule 8 unresolved safety net.

**검증**:
- 4 schema 351 SUPERTYPE 절 모두 0 parse_warnings 로 통과
- 14 unique 패턴 모두 자동 분류 (B0-B7)
- 4 silent fail entity 의 정확한 새 분류 결과 단언으로 박힘
  (`infer::variant::tests::silent_fail_entities_classify_correctly_on_real_schemas`)
- `variants_pending.toml` 미생성 (= 1,780 entity 모두 confident)

**잔존 영역**:
- 미래 schema 진화로 Rule 1 / 1.5 가 못 잡는 더 깊은 anonymous 합성 패턴
  → Rule 8 으로 surface → `variants_pending.toml` 생성 → 사용자가
  `variants_overrides.toml` 에 명시 결정 → 도구 재실행 → unresolved 비어
  짐 → 자동 삭제. 인프라만 준비.

## 3. arena 단계의 ID 분리 신호 미구현

**현재**: 1 group = 1 arena 디폴트. 모든 group 이 confident=0.9 ~ 0.95.

**미흡한 점**: plan 의 arena confidence 신호 3 개 중 ID 분리 (가중치 0.5) 가 미적
용. 실제로는:
- step-io 의 IR 가 같은 EXPRESS type 인 `cartesian_point` 를 두 arena 로 분리
  (`points` + `points_2d`) 한 이유는 PCURVE 의 definitional context 와 일반 3D
  context 를 ID 타입으로 구분하기 위함.
- 현재 분류기는 이 분리를 못 함 → step-io 와 mismatch.

**구현 방향**:
- representation_context 추출: `representation` ATTR 의 type 이 어느 context 를
  받는지 분석. `definitional_representation` 의 후손 안에서 출현 vs 일반
  `geometric_representation_context` 안에서 출현.
- 같은 group 의 entity 들이 두 context 모두에서 등장하면 → 같은 group 을 두
  arena 로 분리 (`<group>` + `<group>_2d` 같은 명명).
- Confidence: clean 분리면 1.0, mixed 면 review.

**난이도**: schema 의 representation_context 의미적 분석이 필요. 단순 ATTR ref
graph 에서 한 단계 더 들어감. plan 의 약 100~200 lines 추가 예상.

## 4. pool 단계 — connected component 가 너무 거친 분류

**현재**: 362 arena → 94 pool (connected component 기반).

**미흡한 점**:
- 거대 component 가 1 개 형성될 가능성 큼 (94 pool 중 한 두 pool 이 200+ arena
  포함). EXPRESS schema 는 cross-reference 가 많아서 connected component 만으로는
  domain 경계를 잡지 못함.
- 모든 결정이 confident → review 가 0 인 것도 의심스러움. 실제로는 community
  detection 의 modularity score 가 borderline 인 arena 들이 review 로 빠져야 함.

**진단 명령**:
```bash
# pool 별 arena 수 분포
grep "^pool = " inferred/pools.toml | sort | uniq -c | sort -rn | head -20
```

**구현 방향**:
- Louvain (or 단순한 greedy modularity-based) community detection 도입.
- 각 arena 의 modularity 기여도 계산 → 신호 점수.
- inbound/outbound ref 비율 신호 정밀화 (현재 same/cross ratio 만 봄).
- 거대 component 안에서 sub-cluster 가 자연스럽게 나오게 됨.

**난이도**: Louvain 구현 또는 외부 crate (`graph` 등) 도입. 구현 100~200 lines
또는 외부 의존 추가.

## 6. 그 외 작은 정리

- `infer arena|pool` 의 `--allow-pending` 같은 옵션이 main.rs 의 단일 flag 로
  처리됨. variant stage 도 같은 파이프라인 안에 있을 때 일관성 있는 옵션 처리
  체계 필요. (variant 단순화 후 variant 영역에선 무관해졌고, arena/pool 영역만
  남음.)
- pending.toml 의 sort order 가 BTreeMap 기반이라 deterministic 하지만, review
  / unresolved 표시 순서가 entity 알파벳순임. confidence 낮은 순으로 정렬하면
  사용자가 "가장 의심스러운 것부터" 검토 가능. (arena/pool 만 해당.)
- override 파일의 stale 검증 메시지에 schema source 정보 추가 (어느 schema 에서
  사라졌는지) — `--diff` 모드와 결합 시 유용. (arena/pool 만 해당.)

## 7. arena 단계 — 거대 enum 의 sub-group 분리 (variant 단순화 부수 효과)

**현재**: variant 단순화 (commit `ac2dbf8`) 후 자동 분류 결과에서 50+ 멤버
enum 6 개 검출:

| enum_root | 멤버 수 |
|---|---|
| data_quality_criterion | 120 |
| generic_expression | 82 |
| geometric_representation_item | 81 |
| shape_aspect | 70 |
| representation_item | 52 |
| characterized_object | 51 |

**미흡한 점**:
- variant 단계는 schema 의 inheritance 를 정직하게 옮기는 것이 책임. 거대
  supertype (`representation_item` 등) 까지 polymorphic context 에 등장하면
  자연스레 거대 enum 으로 분류됨.
- step-io IR 측에서는 이 거대 enum 이 너무 광범위 → 도메인별로 좁은 enum 으로
  분리하는 게 적정.
- §3 (ID 분리) 와 다른 차원의 분리 신호 — context 가 아니라 도메인 (geometry /
  topology / pmi / measure 등) 기반 클러스터링.

**구현 방향**:
- 같은 group 안의 entity 들 사이의 ATTR ref 그래프를 보고, 도메인 sub-cluster
  가 분리되면 group 도 분리.
- 또는 step-io IR 디자이너가 명시적 override 로 supertype 별 sub-group 매핑.
- arena 단계의 explicit override (`[group.X] arena = ...`) 메커니즘이 이미 있음
  → 단지 sub-group 분리 룰만 추가하면 됨.

**난이도**: §3 (ID 분리) 와 함께 다루면 효율적. 분리 신호 모두 arena 단계의
group → arena 매핑 정밀화에 속함.

## 8. ABSTRACT/ONEOF 미명시 concrete supertype 잔존

**현재**: variant 분류기가 schema 의 `ABSTRACT SUPERTYPE` 또는 `SUPERTYPE OF
(ONEOF ...)` 명시를 EnumBase / MergedInto / ComplexSupertype 으로 처리. 명시
없는 concrete supertype 은 SingleStruct 으로 분류되며, 자식들이 자기 자신을
enum_name 으로 가리키면 arena 측 group 충돌 발생.

**예**: `action`, `characterized_object`, `item_defined_transformation` 등 — schema
가 inheritance 만 정의 (자식들이 SUBTYPE OF 명시) 하고 부모 측에 SUPERTYPE OF
절이 없음.

**현재 잔존 카운트**: 24 건 (variant 정밀화 commit 후). 이전 53 건의 약 절반.

**구현 방향 후보**:
- (a) `polymorphic_targets` 에 등장하면서 own_attrs 있는 concrete supertype 도
  ComplexSupertype (또는 새 marker) 로 자동 분류. semantic 결정 필요 — 자기
  instance 가능 + 자식 instance 별개라는 의미 보존하려면 mixin 형태 IR.
- (b) step-io IR 측에서 24 건 수작업 처리. IR 디자이너가 enum 의 한 variant 로
  자기 자신 추가 또는 별도 정책.

**난이도**: 룰 자동화는 schema 신호 부족으로 정밀도 보장 어려움. 24 건이라
수작업 영역 가능성 큼.

## 우선순위 (제안)

작업 순서:
1. **#8 (variant supertype 일관성)** — variant 단계의 룰이 바뀌면 arena 입력도
   변하므로 arena 정밀화보다 먼저 해야 함. ~0.5 일.
2. **#3 (arena ID 분리)** + **#7 (거대 enum sub-group 분리)** — 묶어서 arena 단계
   분리 신호 정밀화. variant 안정화 후 진행. ~1~2 일.
3. **#4 (pool community detection)** — 진짜 의미 있는 pool 결정. ~1~2 일.
4. **#6 (작은 정리)** — 우선순위 낮음.

## 검증 방향

정밀화 작업의 검증 게이트:
- arena: 일부 group 이 ID 분리로 두 arena 로 나뉘는 결과 확인 (예:
  `points` + `points_2d`). 거대 enum (50+ 멤버 6 개) 도 sub-group 으로 분해
  되어 enum 당 멤버 수 분포가 평탄화 (p95 < 30 정도 목표).
- variant supertype 처리: `surface` / `representation_item` 등 supertype entity
  의 분류가 일관 (둘 다 in_enum 또는 둘 다 single_struct, 또는 새 variant).
  arena 의 group 충돌 0 건.
- pool: 거대 단일 component 가 sub-cluster 로 분해되어 pool 수가 ~15~20 정도로
  안정. step-io 의 현재 `GeometryPool` / `TopologyPool` 등과 정합 비교.

각 단계의 `--idempotency` 검증, override 작성 후 cycle 검증 (override → 재실행
→ pending 단조 감소) 도 정밀화 후 한 번 더 수행. (variant 단계는 override
폐기 후 재실행마다 schema 결과로 deterministic.)
