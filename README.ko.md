# rxls

[![English](https://img.shields.io/badge/Language-English-1D5FBF.svg)](README.md)
[![한국어](https://img.shields.io/badge/Language-%ED%95%9C%EA%B5%AD%EC%96%B4-0F766E.svg)](README.ko.md)

**Rust 네이티브 스프레드시트 툴킷.** `.xls`, `.xlsx`, `.xlsb`, `.ods`를 하나의
타입 셀 모델로 통합해 읽습니다. 서식이 적용된 `.xlsx`를 생성하고, 수정하지 않은
패키지 구성요소는 그대로 둔 채 `.xlsx`/`.xlsm`을 편집합니다.

[![Crates.io](https://img.shields.io/crates/v/rxls.svg)](https://crates.io/crates/rxls)
[![Docs.rs](https://docs.rs/rxls/badge.svg)](https://docs.rs/rxls)
[![CI](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml/badge.svg)](https://github.com/HyunjoJung/rxls/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![MSRV](https://img.shields.io/badge/MSRV-1.85-orange.svg)

코어 라이브러리는 JVM이나 Apache POI를 요구하지 않고, Office 자동화를
사용하거나 별도 프로세스를 실행하지 않습니다. 오래된 한국어 cp949 통합문서부터
신뢰할 수 없는 업로드까지 받아야 하는 문서 처리 환경을 대상으로 하며, 잘못된
입력으로 panic하지 않습니다.

```sh
cargo add rxls@0.1.3 --features full
```

## 핵심 기능

| 형식 | 읽기 | 쓰기 | 원본 패키지 보존 편집 | 표시 값 검증 |
|---|:---:|:---:|:---:|---|
| `.xls` (BIFF8/5/7) | ✓ | - | - | 414/414, `xlrd` 비교 |
| `.xlsx` | ✓ | ✓ 서식 포함 | ✓ 수정하지 않은 파트 보존 | 387/387, `openpyxl` 비교 |
| `.xlsm` | ✓ | - | ✓ VBA 보존 | OOXML 행에 포함 |
| `.xlsb` | ✓ | - | - | 18/18, `pyxlsb` 비교 |
| `.ods` | ✓ | - | - | 14/14, 범위를 제한한 ODF XML 비교 |

다음 기능도 포함합니다.

- 결정론적 수식 평가 MVP
- CSV, HTML, Markdown 내보내기
- 기계 판독 가능한 통합문서 진단
- CLI와 독립 실행형 WASM 어댑터
- 타입 기반 행 역직렬화와 선택적 `chrono` 변환

### 릴리스 현황

| 릴리스 | 테스트 | 공개 코퍼스 |
|---|---|---|
| `0.1.3` · MIT · MSRV 1.85 | 릴리스와 동일한 소스에서 all-target/all-feature 테스트 1,092개 | 916개 파일 · 868개 열기 성공 · 예상 거절 48개 · 예상 밖 결과 0개 |

[crates.io](https://crates.io/crates/rxls/0.1.3)와
[docs.rs](https://docs.rs/rxls/0.1.3/rxls/)에 게시되어 있습니다. 단일 리비전에
연결된 [52개 자산의 릴리스 증거 묶음](https://github.com/HyunjoJung/rxls/releases/tag/v0.1.3)도 제공합니다.

## 데모와 아키텍처

| 한국어 시연 | 영어 시연 |
|---|---|
| [![rxls 0.1.3 한국어 실제 시연](.github/assets/rxls-demo-thumbnail.png)](https://youtu.be/IzmFd_ARh1A) | [![rxls 0.1.3 English live demo](.github/assets/rxls-demo-thumbnail-en.png)](https://youtu.be/Z7tNhqMdCVU) |
| [2분 54초 한국어 시연 보기](https://youtu.be/IzmFd_ARh1A) | [2분 53초 영어 시연 보기](https://youtu.be/Z7tNhqMdCVU) |

두 영상 모두 공개된 릴리스를 그대로 사용합니다. 실제 `rxls` CLI로
BIFF5/cp949 통합문서를 읽고, 네 가지 형식을 공통 모델로 열어 서식이 적용된 6행
XLSX 보고서를 생성합니다. Excel 16에서 보고서의 표, 필터, 캐시된 `SUM` 수식,
데이터 유효성 검사, 차트를 확인한 뒤 `openpyxl 3.1.5`로 다시 열어 검증합니다.
읽기 명령은 정확한 `v0.1.3` CLI인
[`e1390e5`](https://github.com/HyunjoJung/rxls/commit/e1390e5aa349fbf933c39bccda400a4a2ee1d814)를
사용하며, 저장소에 포함된 보고서 드라이버도 같은 체크아웃의 라이브러리를
호출합니다.

[한국어 자막](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/rxls-2026-oss-contest-demo.ko.srt) ·
[영어 자막](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/rxls-2026-oss-contest-demo.en-US.srt) ·
[한국어 빌드 영수증](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-verification.json) ·
[영어 빌드 영수증](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-verification.en-US.json) ·
[한국어 QA](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-qa.json) ·
[영어 QA](https://github.com/HyunjoJung/rxls/releases/download/oss-contest-2026-demo/video-qa.en-US.json) ·
[미디어 릴리스](https://github.com/HyunjoJung/rxls/releases/tag/oss-contest-2026-demo)

![rxls 아키텍처: 신뢰할 수 없는 바이트를 범위가 제한된 형식 파서와 하나의 타입 모델을 거쳐 공개 인터페이스로 전달](.github/assets/rxls-architecture.png)

공모전 미디어 릴리스는 변경할 수 없는 `v0.1.3` 릴리스 증거 묶음과 의도적으로
분리했습니다.

## 빠른 시작

### 읽기

검색과 인덱싱에는 일반 텍스트를, 구조를 유지한 읽기에는 타입 셀을 사용할 수
있습니다.

```rust
let bytes = std::fs::read("book.xls")?;
let text = rxls::extract_text(&bytes)?;

let wb = rxls::Workbook::open(&bytes)?;
for sheet in &wb.sheets {
    if let Some(rxls::Cell::Date(serial)) = sheet.cell(0, 0) {
        println!("A1의 Excel date serial: {serial}");
    }
    for (row, col, cell) in sheet.cells() {
        // Cell::Text/Number/Date/Bool/Error/Formula
    }
}
```

`Workbook::open`은 컨테이너 시그니처를 자동으로 판별합니다. Cargo에서 해당
형식 기능을 켜면 같은 호출로 네 가지 형식을 모두 처리합니다.

### `.xlsx` 생성

```rust
use rxls::{CellStyle, HAlign, Workbook};

let mut wb = Workbook::new();
let sheet = wb.add_sheet("운영보고서");

let header = CellStyle::new()
    .bold()
    .fill([0xDD, 0xEB, 0xF7])
    .align(HAlign::Center)
    .wrap();

sheet.write_styled(0, 0, "항목", &header);
sheet.write_styled(0, 1, "금액", &header);
sheet.write_url(1, 0, "https://example.com/reports/2026-07", "7월 운영 현황");
sheet.write_styled(1, 1, 150_000_000.0, &CellStyle::new().num_fmt("₩#,##0"));
sheet.set_col_width(0, 42.0);
sheet.freeze_panes(1, 0);
sheet.autofilter(0, 0, 1, 1);

std::fs::write("report.xlsx", wb.to_xlsx())?;
```

### CLI

```sh
cargo install rxls --version =0.1.3 --locked

rxls info book.xlsx
rxls diagnose book.xlsx
rxls csv book.xlsx --sheet 0 --max-output-bytes 1048576
rxls compare before.xlsx after.xlsx --limit 50
```

정상적인 `--help`와 명령 결과는 stdout으로, 사용법과 실행 오류는 stderr로
출력됩니다. 종료 상태 분류와 diagnose JSON 스키마 변경은 호환성 정책으로 관리되는
공개 동작입니다.

## Cargo 기능

| 기능 | 기본값 | 제공 범위 |
|---|:---:|---|
| `cli` | 예 | `rxls` 바이너리 빌드 |
| `xlsx` | 예 | XLSX/XLSM 읽기, XLSX 쓰기, 패키지 보존 편집 |
| `xlsb` | 아니요 | XLSB 리더, `xlsx` 패키지 지원 활성화 |
| `ods` | 아니요 | ODS 리더 |
| `serde` | 아니요 | 타입 기반 행 역직렬화 |
| `chrono` | 아니요 | 날짜·시간 및 duration 변환 |
| `full` | 아니요 | 모든 라이브러리 형식/데이터 기능, `cli`는 의도적으로 제외 |

레거시 XLS 리더는 항상 사용할 수 있습니다. XLS 전용 라이브러리 빌드에는
`default-features = false`, 모든 리더와 타입 데이터 도우미에는
`features = ["full"]`을 사용합니다. 최소 지원 Rust 버전은 1.85입니다.

## 릴리스 계약

> `0.1.3`은 크레이트, 태그가 가리키는 소스, GitHub Release 번들, SBOM,
> 체크섬, `provenance`가 릴리스 매니페스트를 통해 모두 같은 리비전에 연결될
> 때만 승인됩니다.

`v0.1.3` 태그와 게시된 패키지는 변경할 수 없습니다. 이후 `main`에는 문서나
미출시 작업이 포함될 수 있습니다. 릴리스 계약을 평가할 때는 버전이 명시된
패키지를 사용하고, `main`을 평가할 때는 체크아웃한 소스를 직접 빌드하십시오.

게시 전후 게이트는 리더와 수식의 정확성, 패키지 보존 방식의 XLSX/XLSM 편집,
CLI, JSON, 코어 WASM, 공개 코퍼스 일치도, 보안 분석, 퍼징, 성능 예산,
SBOM/`provenance`, 릴리스 패키지 설치를 검증합니다.

## 공개 코퍼스 증거

2026-08-22에 고정한 수집 규칙은 Apache POI와 calamine의 지정된 업스트림
커밋에서 916개 파일을 선택합니다.

| 형식 | 전체 | 독립 비교 결과 |
|---|---:|---|
| `.xls` | 448 | 비교 가능 414개 모두 99% 이상, 평균 100% |
| `.xlsx`/`.xlsm` | 431 | 비교 가능 387개 모두 99% 이상, 평균 100% |
| `.xlsb` | 21 | 비교 가능 18개, 평균 일치율 100% |
| `.ods` | 16 | 비교 가능 14개, 평균 재현율 100% |

`rxls corpus-report`는 868개를 열었습니다. 나머지 48개는 암호화된 입력,
지원하지 않는 레거시 BIFF, 잘못 구성된 컨테이너, 구조적으로 잘못된 BIFF
스트림, 잘못된 OOXML 패키지 관계로 사전에 분류한 예상 거절입니다. 예상 밖
실패와 예상 밖 수용은 모두 0건입니다.

릴리스 주장은 공개되어 있고 재현 가능한 테스트 픽스처와 코퍼스에만 의존합니다.
자세한 baseline, oracle 버전, input manifest SHA-256은
[영문 README](README.md)의 Public corpus evidence 절을 참고하십시오.

## 안전성과 설계 원칙

- 확장자가 아니라 OLE2/ZIP 등 컨테이너 시그니처로 형식을 판별합니다.
- 입력·할당·재귀·출력 크기에 상한을 둡니다.
- `FILEPASS` 암호화 문서는 암호문을 내보내지 않고 `Error::Encrypted`로
  거절합니다.
- 잘못된 구조는 범위가 제한된 명시적 복구 경로로 처리하거나 타입 오류를
  반환합니다.
- 매크로와 외부 링크를 실행하지 않습니다.
- `#![forbid(unsafe_code)]`, CodeQL, 의존성 정책, 퍼징 게이트를 적용합니다.
- 복구 경로가 사용됐다는 사실은 감사 신호일 뿐이며, 원본 컨테이너의 완전성을
  보장하지 않습니다.

## 지원 범위

### 읽기

날짜, 시간, 백분율은 보존된 서식 메타데이터를 사용해 표시합니다. 사용자 지정
서식은 섹션, 조건, 색상, 로캘/통화 표시, 그룹화/배율, 분수, 지수 표기법,
경과 시간 토큰, 리터럴, 이스케이프, 텍스트 자리표시자를 지원합니다. 수식
재평가는 결정론적 MVP로 제한되며, 범위 밖 수식에는
`FormulaUnsupportedReason`을 반환하고 값을 추측하지 않습니다.

### 기존 파일 편집

패키지 구조를 보존하는 편집은 `.xlsx`와 `.xlsm`만 지원합니다. atomic batch,
셀·수식·범위 수정, 문서·이름·시트·레이아웃·틀·인쇄 영역 메타데이터, 시트
추가/이름 변경/삭제, 병합, 레거시 메모, 하이퍼링크, 정확한 범위 유효성 검사,
기존 표의 마지막 행을 안전하게 조정하는 기능을 제공합니다. 패키지에서 선언된
파트 중 수정하지 않은 부분과 VBA 콘텐츠는 바이트 단위로 그대로 보존합니다.

행이나 열의 삽입·삭제는 지원하지 않으며, 안전하지 않은 구조적 의존성을 추측해
복구하지 않습니다.

### `.xlsx` 생성

글꼴, 채우기, 테두리, 숫자 서식, 정렬, 병합, 너비/높이, 틀 고정, 자동 필터,
하이퍼링크, 페이지 설정, 보호, 탭 색상, 데이터 유효성 검사, 조건부 서식,
PNG/JPEG 이미지, 차트, 스파크라인, 워크시트 표, 서식 있는 문자열, 레거시
메모/주석을 지원합니다. 피벗 테이블, 스레드형 주석, 매크로 생성은 범위
밖입니다.

### 내보내기, 진단, WASM

워크시트는 CSV, HTML, Markdown으로 내보낼 수 있습니다. `WorkbookReport`와
`rxls diagnose`는 시트·셀·수식 수, 문서 속성, 기능 목록, `parse_provenance`를
JSON으로 제공합니다. `bindings/wasm`은 Node/브라우저 엔트리 포인트,
TypeScript 선언, 구조화된 `RxlsError`, 32 MiB 입력 제한을 제공합니다.

## 재현

아래 명령은 비공개 데이터 없이 깨끗한 체크아웃에서 실행됩니다.

```bash
python3 -m pip install \
  "CairoSVG==2.9.0" "numpy==2.4.4" "openpyxl==3.1.5" \
  "Pillow==12.3.0" "pyxlsb==1.0.10" "xlrd==2.0.2" "odfpy==1.4.1"
python3 scripts/public_hygiene_audit.py
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --locked -- -D warnings
RXLS_REQUIRE_OPENPYXL=1 cargo test --all-targets --all-features --locked
cargo test --no-default-features --all-targets --locked
cargo test --doc --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features --locked
python3 -m unittest discover -s scripts -p "test_*.py"
cargo package --locked
cargo publish --dry-run --locked
```

전체 916개 공개 코퍼스 실행 방법과 결정론적 릴리스 워크플로는
[영문 README](README.md#reproduce)에 유지합니다.

## 실험적 렌더링 워크스페이스

렌더러와 `@rxls/render-worker`는 게시된 `rxls 0.1.3` 코어 계약에 포함되지
않는 별도의 소스 전용 트랙입니다. 코어 크레이트의 리더/라이터/CLI/WASM
릴리스 주장과 구분됩니다. 빌드 방법, 제한 사항, 글꼴 격리, 페이지 나누기,
배포 게이트는 [렌더러 가이드](render/README.md)와
[워커 패키지 가이드](bindings/render-wasm/README.md)를 참고하십시오.

## 기여

이슈와 pull request를 환영합니다. [CONTRIBUTING.md](CONTRIBUTING.md)는 공개
항목 문서화, 최소 의존성, 사양 인용, 처리 범위 제한, PR 전 로컬 게이트를
설명합니다. [Code of Conduct](.github/CODE_OF_CONDUCT.md)와
[Security Policy](.github/SECURITY.md)도 함께 적용됩니다.

## 라이선스

[MIT License](LICENSE)로 배포합니다. 서드파티 의존성 라이선스는
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md)에 정리했습니다. 공개된
[MS-XLS], [MS-XLSB], [MS-CFB], [ECMA-376], [ODF] 사양만 구현하며 Microsoft
소스를 포함하지 않습니다.

Microsoft와 Excel은 Microsoft 그룹사의 상표입니다. 이 프로젝트는 Microsoft와
제휴 관계가 없으며 Microsoft의 승인이나 후원을 받지 않습니다.

[MS-XLS]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xls/
[MS-XLSB]: https://learn.microsoft.com/en-us/openspecs/office_file_formats/ms-xlsb/
[MS-CFB]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-cfb/
[ECMA-376]: https://ecma-international.org/publications-and-standards/standards/ecma-376/
[ODF]: https://docs.oasis-open.org/office/OpenDocument/v1.3/
