<p align="center"><img src="assets/ko/banner.svg" alt="voltdir: 디스크 전체를 한 번의 호출로." width="100%"></p>

**다른 언어로 읽기:** [English](README.md) · [正體中文](README.zh-TW.md) · [简体中文](README.zh-CN.md) · 한국어 · [日本語](README.ja.md) · [Tiếng Việt](README.vi.md)

**한 번 스캔하면, 그다음부터는 몇 번을 물어봐도 질문당 약 1초.**

**AI 에이전트를 위한 Windows 디스크 분석 도구.** 한 번의 호출로 드라이브를 무엇이 채우고 있는지 알 수 있습니다. 가장 큰 폴더, 공간을 가장 많이 차지하는 확장자, 빈 파일의 위치, 최근 며칠 사이에 늘어난 것까지. 읽기 전용, 실행 파일 하나, 형식이 고정된 JSON 출력.

| `C:\` 전체 (관리자) | 메모리, 파일 100만 개 | 단일 스캔, 파일 100만 개 | 에이전트의 연속 질문 |
|:---:|:---:|:---:|:---:|
| **5.7배 빠름**<br>6.6초 대 37.5초 | **11배 적음**<br>14 MB 대 163 MB | **최대 117배 빠름**<br>Get-ChildItem 대비. 가장 빠른 .NET 코드 대비로는 2.5배 | **17.4배 빠름**<br>연속 30개 질문, 질문당 약 1초 |

> **첫 번째 질문은 어떤 스캔이든 똑같이 걸립니다.** voltdir는 폴더를 스캔하고 인덱스를 만드는데, 이 부분은 잘 짠 Python 스크립트도 더 느리지 않습니다(파일 260만 개에서 0.9배). 이점은 그다음 모든 질문에 있습니다. 아직 앞서지 못한 부분은 아래에 적어 두었습니다.

**`C:\` 전체 (관리자): 5.7배 빠름, 6.6초 대 37.5초.**

![C:\ 전체, 파일 약 310만 개](assets/ko/chart-mft.svg)

**메모리, 파일 100만 개 폴더: 약 11배 적음.**

![최대 메모리: 파일 100만 개가 든 폴더](assets/ko/chart-memory.svg)

**파일 100만 개 폴더의 단일 스캔: 흔히 쓰는 Get-ChildItem 스크립트보다 최대 117배, 가장 빠른 .NET 코드보다 2.5배 빠름.**

![단일 스캔: 파일 100만 개가 든 폴더](assets/ko/chart-scan.svg)

**에이전트가 당신 대신 연속으로 질문할 때: 30개 질문에서 17.4배 빠름, 질문당 약 1초.**

![후속 질문: 한 번 스캔하고, 여러 번 질문하기](assets/ko/chart-followups.svg)

수치는 실제 폴더에서 잘 작성된 비교 대상과 견주어 측정한 것입니다. 모든 수치(**아직 앞서지 못한 부분 포함**)는 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)에 있고, 모든 차트는 [벤치마크 페이지](BENCHMARKS.ko.md)에 있으며, [`benchmarks/`](benchmarks/README.md)의 스크립트로 내 컴퓨터에서도 재현할 수 있습니다.

- **읽기 전용:** 이름, 크기, 시간, 속성만 읽습니다. 파일 내용을 읽지 않고, 삭제하거나 옮기지 않으며, 네트워크에 연결하지 않고, 스스로 권한을 높이지도 않습니다.
- **`voltdir.exe` 하나** (x64, 약 0.75 MB): C 런타임을 정적으로 링크하고 Windows 기본 DLL만 가져옵니다. 따로 설치할 의존성이 없습니다.
- **에이전트용 스킬:** [`skills/voltdir/SKILL.md`](skills/voltdir/SKILL.md) (약 5,600자)와 빌드된 실행 파일이 함께 들어 있으며, 첫 단계는 항상 `overview`입니다.
- 상태: **v1 후보**, Windows x64 전용.

## 평소 말투 그대로 말하세요

검색 패턴을 쓸 필요가 없습니다. 하고 싶은 일을 에이전트에게 말하면 에이전트가 검색으로 바꾸고, voltdir는 인덱스에서 답합니다. 파일 내용은 하나도 읽지 않습니다.

| 이렇게 말하면 | voltdir가 찾는 것 |
|---|---|
| “카카오톡이나 LINE이 자동으로 내려받은 PDF를 전부 찾아서 정리해 줘” | 지정한 모든 드라이브(`C:`, `F:` 모두)의 PDF: 어디서 왔는지(메신저 폴더, 다운로드, 바탕 화면), 얼마나 오래됐는지, 크기, 청구서나 계약서처럼 보이는 것, 중복된 것을 결정용 목록으로 |
| “휴대폰에서 옮긴 사진 좀 정리해 줘” | 카메라와 휴대폰으로 찍은 사진(`IMG_0001.JPG`, `PXL_…heic`)을 폴더, 크기, 날짜와 함께 |
| “스크린샷 좀 정리하고 싶어” | `스크린샷 2026-01-02.png` 같은 이름의 파일을 저장된 위치와 상관없이 모두 |
| “다운로드 폴더에 중복이 많지 않아?” | `보고서 (1).pdf`, `보고서 - 복사본.docx` 같은 사본을 후보 목록으로 |
| “안전하게 지울 수 있는 게 뭐야?” | 남은 임시 파일(`.bak`, `.old`, `.tmp`, 끝나지 않은 다운로드), 오래된 설치 파일, 빈 폴더를 크기와 오래된 정도와 함께. 결정은 당신이 하며, voltdir는 아무것도 삭제하지 않습니다 |
| “세금 신고용으로 영수증 찾아 줘” | 이름에 ‘영수증’이나 ‘청구서’가 들어간 파일. 아무것도 열지 않습니다 |
| “이 컴퓨터에 프로젝트가 몇 개 있어?” | `package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`가 들어 있는 폴더 |

에이전트가 실제로 쓰는 검색 패턴은 테스트를 거쳤고 [`skills/voltdir/reference.md`](skills/voltdir/reference.md)에 정리되어 있습니다.

### 메신저가 자동으로 내려받은 PDF

카카오톡, LINE, WeChat 같은 메신저는 받은 PDF를 하나하나 자기 폴더에 저장하지만, 거기를 들여다보는 사람은 거의 없습니다. 실제 컴퓨터 한 대에서는 드라이브에 있는 PDF 728개 중 371개가 WeChat 폴더에 있었습니다. 한 번 스캔한 뒤 차례로 물어보세요. 모두 몇 개이고 얼마나 큰지, 어느 앱에서 왔는지, 1년 넘은 것은 무엇인지, 청구서나 계약서처럼 보이는 것은 무엇인지, 중복으로 내려받은 것은 무엇인지. 이런 질문 7개가 스캔 후 합계 1.25초 걸렸습니다. PDF 검색은 이름 패턴이 아니라 확장자를 쓰기 때문입니다. 그다음 에이전트가 폴더 구조와 이동 목록을 제안합니다. voltdir는 이름만 읽으므로 당신이 승인하기 전에는 아무것도 열거나 옮기거나 삭제하지 않습니다.

## 에이전트를 위해 만들었습니다

- **보지 못한 것을 알려 줍니다.** 읽지 못한 폴더는 `coverage`에 나열되고 종료 코드가 2가 되므로, 불완전한 답이 완전한 것처럼 넘어가지 않습니다.
- **말에 휘둘리지 않습니다.** 파일 이름은 데이터일 뿐입니다. 제어 문자와 오른쪽에서 왼쪽 방향 속임수는 이스케이프되므로, ‘이전 지시를 무시하고……’라는 이름의 파일도 그저 이름으로 남습니다.
- **답이 얼마나 최신인지 알려 줍니다.** stderr에 `index: used (built 12 minutes ago)`가 나옵니다.
- **어떤 에이전트든 찾을 수 있습니다.** Claude 스킬과 플러그인, 그리고 [`AGENTS.md`](AGENTS.md)를 읽는 에이전트를 위한 파일이 있습니다. 평범한 명령줄이며 상주하는 서버가 없습니다.

---

## 설치

### Claude Code 플러그인으로

```text
/plugin marketplace add CubeLink-oss/voltdir
/plugin install voltdir@voltdir
```

플러그인에는 스킬과 빌드된 `voltdir.exe`([`skills/voltdir/bin/`](skills/voltdir/bin))가 들어 있습니다. "디스크가 왜 꽉 찼지?", "큰 파일 찾아 줘", "이번 주에 뭐가 늘었어?" 같은 질문에 에이전트가 자동으로 사용합니다.

### 실행 파일만 따로

[`skills/voltdir/bin/voltdir.exe`](skills/voltdir/bin)를 아무 폴더에나 복사하면 바로 쓸 수 있으며, 설치는 필요 없습니다. 아직 코드 서명이 없어서 처음 실행할 때 Windows SmartScreen 경고가 나타날 수 있습니다. 실행하기 전에 이 소스로 빌드한 파일인지 확인할 수 있습니다. [포함된 실행 파일 검증하기](#포함된-실행-파일-검증하기)를 참고하세요.

---

## 빠른 시작

```powershell
# 첫 호출: 합계, 가장 큰 폴더와 파일, 확장자, 오래 수정되지 않은 큰 파일, 빈 파일,
# 그리고 바로 실행할 수 있는 next 명령
voltdir overview D:\Projects

# 같은 스캔에서 가벼운 인덱스를 저장해 두면 후속 질문에 다시 스캔할 필요가 없습니다
voltdir overview D:\Projects --save-index C:\Temp\p.idx
voltdir report --index C:\Temp\p.idx --top-dirs 20 --by-ext 15

# find나 diff가 필요할 때만 전체 스냅샷을 저장합니다. 이름 인덱스도 함께 쓰므로
# 이후의 find --snapshot은 1초 안에 답합니다 (이전 스냅샷은 voltdir index 파일 로 보완)
voltdir overview D:\Projects --save-snapshot C:\Temp\p.ndjson
voltdir find --snapshot C:\Temp\p.ndjson --ext .log --min-size 100M --sort size --limit 20
voltdir diff C:\Temp\monday.ndjson C:\Temp\friday.ndjson --match-roots
```

드라이브 전체를 스캔할 때는 PowerShell을 **관리자 권한으로 실행**하세요. 그래야 NTFS 마스터 파일 테이블(MFT)을 직접 읽는 `mft` 엔진을 쓸 수 있습니다. 관리자가 아니면 자동으로 `walk` 엔진으로 바뀌고 일부 시스템 폴더는 읽지 못하며, 읽지 못한 위치는 출력의 `coverage`에 모두 나열됩니다.

---

## 벤치마크

모든 차트(메모리, 드라이브 전체, 에이전트의 연속 질문, 단일 스캔, 일반 Python 스크립트와의 비교)는 **[벤치마크 페이지](BENCHMARKS.ko.md)**에, 수치는 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)에 있습니다.

### 아직 앞서지 못한 부분

| 항목 | 현황 |
|---|---|
| 첫 번째 질문 | 일반 사용자 권한의 스캔에서는 잘 짠 Python 스크립트보다 빠르지 않습니다. 둘 다 같은 NTFS 디렉터리 열기 비용을 기다립니다(파일 260만 개에서 0.9배). 이점은 그다음 질문에 있습니다. |
| 정규식 검색 | 여러 후보가 있는 검색은 이름 260만 개에서 약 2초 걸립니다. 이름 비교 처리는 아직 최적화하지 않았습니다. |
| 관리자 권한 없는 단일 스캔 | 파일 100만 개에서는 가장 빠른 .NET 구현보다 약 2.5배 빠르지만, `C:\` 전체에서는 오히려 느립니다(0.78배). 병목은 NTFS에서 디렉터리를 여는 비용입니다 |
| 토큰 사용량 | 비교 대상과 비슷하며 voltdir 쪽이 4~10% 더 썼습니다. 에이전트 호출마다 약 7.3만 토큰의 고정 비용이 있어 어떤 도구로도 줄일 수 없습니다 |
| 관리자 권한으로 드라이브 전체를 스캔할 때의 메모리 | 186 MB로 .NET 스트리밍 구현(177 MB)과 비슷하며, 앞서지는 못합니다 |

이유는 [`benchmarks/RESULTS.md`](benchmarks/RESULTS.md)에 설명되어 있습니다.

## 잘 되는 것과 주의할 것

**잘 되는 것**

- **믿을 수 있는 개수.** junction과 하드 링크를 처리하므로 합계가 실제와 맞습니다([벤치마크 페이지](BENCHMARKS.ko.md) 참고).
- **조용히 빠뜨리지 않습니다.** 읽지 못한 폴더가 있으면 종료 코드가 2이고 `coverage`에 모두 나열됩니다.
- **후속 질문이 저렴합니다.** 인덱스가 있으면 질문 하나에 약 0.05초입니다.
- **출력이 안정적입니다.** 같은 입력과 같은 엔진이면 JSON이 바이트 단위로 같습니다.

**독립 평가에서 발견되어 0.0.2에서 수정했습니다.** 에이전트가 실제로 쓰는 방식으로 voltdir를 테스트했고, 버그로 확인된 것은 모두 고쳤습니다.

- 슬래시로 쓴 패턴(`--exclude "*/node_modules/*"`, `--path "*/src/*"`)은 이전에는 아무것도 일치하지 않았고, 그것도 조용히 그랬습니다. 명령은 성공했지만 아무것도 제외되지 않았습니다. 이제 모든 패턴에서 `/`와 `\`는 같은 뜻입니다.
- 어떤 경로와도 일치하지 않는 `--exclude` 패턴은 이제 stderr에 경고를 출력합니다. 오타가 눈에 띄지 않고 지나가는 일은 없습니다.
- `--json`은 모든 명령에서 받아들입니다(아무 영향도 없으며 출력은 항상 JSON입니다). `--follow-links none`도 받아들입니다(그것이 기본값입니다).
- `--help`에 드라이브 전체 스캔의 종료 코드 2가 정상이라는 점과 `mft` 엔진의 요구 사항을 적었습니다.

**알아 둘 것**(설계에 따른 동작이며 버그가 아닙니다)

| 동작 | 이유 | 대처 |
|---|---|---|
| `find --snapshot ... --exclude`는 오류가 남 | 스캔 옵션은 스캔할 때만 적용되고 스냅샷을 읽을 때는 적용되지 않습니다 | 스캔할 때 제외(`scan --exclude`)하거나 `--path`로 거르기 |
| 드라이브 전체 스캔의 종료 코드는 2 | 휴지통 같은 시스템 폴더는 읽을 수 없습니다. 결과는 유효하며 `coverage`에 나옵니다 | 스크립트에서 2를 "경고와 함께 완료"로 처리하고 실패로 보지 않기 |
| `find --limit`은 경로 순서로 앞쪽을 남김(기본 100) | 따로 지정하지 않으면 결과는 경로순으로 정렬됩니다 | `--sort size`를 쓰고 `matched_total`과 목록 개수를 비교 |
| 속도 우위는 트리의 모양에 따라 다름 | 파일 100만 개 폴더에서는 Python 반복문의 약 5배, 개발 작업 공간이나 드라이브 전체에서는 약 2배([벤치마크 페이지](BENCHMARKS.ko.md) 참고) | 자기 폴더에서 벤치마크를 실행해 보기 |

---

## 명령

| 명령 | 용도 | 출력 |
|---|---|---|
| `overview <PATH>... \| --snapshot <FILE> \| --index <FILE>` | **자주 묻는 질문에 한 번에 답합니다**(약 3 KB): 합계, 가장 큰 폴더와 파일, 확장자 분포, 오래 수정되지 않은 큰 파일, 빈 파일과 빈 폴더, 읽지 못한 위치, 바로 실행할 수 있는 `next` 명령. 옵션: `--save-index`, `--save-snapshot`, `--rank physical\|logical`, `--dir-rank rollup\|direct`, `--detail concise\|full` | JSON 문서 1개 (`voltdir.overview/1`) |
| `report <PATH>... \| --snapshot <FILE> \| --index <FILE>` | 직접 집계: `--top-files`, `--top-dirs`, `--by-ext`, `--many-files` | JSON 문서 1개 (`voltdir.report/1`) |
| `find <PATH>... \| --snapshot <FILE>` | 이름(와일드카드 또는 `--name-regex`), 경로, 종류, 확장자, 크기, 날짜, 비어 있는지 여부로 검색 | JSON 문서 1개 (`voltdir.find/1`) |
| `diff <A> <B>` | 두 스냅샷 비교: 커진 것, 줄어든 것, 추가된 것, 사라진 것. 경로가 다른 스냅샷끼리는 `--match-roots`(또는 `--a-root` / `--b-root`)를 붙입니다 | JSON 문서 1개 (`voltdir.diff/1`) |
| `scan <PATH>... --out <FILE>` | **스냅샷**만 출력합니다. 수백 MB가 될 수 있어 `--out`은 필수이며, 표준 출력으로 내보내려면 `--out -`라고 명시합니다 | NDJSON (`voltdir.snapshot/1`) |
| `info` | 환경과 기능: 버전, 권한 상승 여부, 사용 가능한 엔진, 볼륨 | JSON 문서 1개 |

자주 쓰는 옵션: `--out <FILE|->`, `--engine auto|mft|walk`, `--threads <N>`, `--exclude <GLOB>`, `--follow-links mount|symlink|junction`, `--hardlinks dedupe|none`, `--timeout-sec <N>`, `-q`. 전체 문법은 `voltdir --help` 또는 `voltdir help <명령>`으로 확인하세요.

## 두 가지 엔진

| | `walk` | `mft` |
|---|---|---|
| 관리자 권한 | 필요 없음 | **필요**(볼륨의 MFT를 읽음) |
| 범위 | 모든 폴더, 모든 파일 시스템 | 로컬 NTFS 드라이브의 **루트**만(예: `C:\`) |
| `C:\` 전체(파일 약 310만 개) | 약 54초 | **약 6.6초, 최대 186 MB** |
| 보이는 범위 | 권한의 제약을 받으며 `$MFT` 같은 예약 항목은 보이지 않음 | 예약 항목(`reserved: true`로 표시)과 `System Volume Information`의 내용까지 보임 |

`--engine auto`(기본값)는 조건이 맞으면 `mft`를 쓰고, 그렇지 않으면 `walk`로 바꾸면서 이유를 stderr에 알립니다. `--engine mft`는 엄격 모드로, 쓸 수 없으면 종료 코드 3으로 끝납니다.

## 종료 코드

| 코드 | 의미 |
|---|---|
| 0 | 완전히 성공 |
| 1 | 사용법 오류. 쓸 수 없는 스냅샷이나 인덱스, 루트가 겹치지 않는 두 스냅샷을 `--match-roots` 없이 `diff`한 경우를 포함합니다 |
| 2 | 결과는 유효하지만 **일부 폴더를 읽지 못했습니다**(대개 액세스 거부). 위치는 `coverage`에 나열되며, **완전한 결과로 취급하면 안 됩니다** |
| 3 | 지정한 엔진을 쓸 수 없습니다(관리자가 아닌데 `--engine mft`를 지정했거나 NTFS가 아닌 볼륨 등) |
| 4 | 출력 파일 쓰기에 실패했습니다(쓰다 만 파일은 남기지 않습니다) |
| 5 | 시간 초과 또는 취소(스캔한 부분까지는 출력합니다) |

## 출력 형식 요점

- 경로는 `\\?\` 접두사가 없는 Win32 형식입니다. 짝이 맞지 않는 UTF-16 서로게이트는 `\uXXXX`로 이스케이프하므로 손실되지 않습니다. 제어 문자와 양방향 텍스트 문자는 항상 이스케이프해서, 조작된 파일 이름으로 에이전트에 프롬프트 인젝션을 시도할 수 없게 합니다.
- 시간은 UTC 기준 RFC 3339, 크기는 바이트 단위 정수입니다. `size_logical`은 파일 끝 위치, `size_physical`은 디스크에 실제로 할당된 공간입니다.
- 출력 순서는 고정입니다. 같은 입력과 같은 엔진이면 출력이 바이트 단위로 같습니다.
- **하드 링크**는 실제 크기에서는 한 번만, 논리 크기에서는 이름마다 셉니다.

## 빌드와 테스트

Rust 1.94 이상(MSVC 툴체인)이 필요합니다.

```powershell
cargo build --release          # target\release\voltdir.exe 생성
.\scripts\ci.ps1               # 포맷, clippy, 전체 테스트, 라이선스 확인, 벤치마크 스크립트 자체 테스트, 스킬 확인
```

벤치마크 재현 방법은 [`benchmarks/README.md`](benchmarks/README.md)를 참고하세요.

## 포함된 실행 파일 검증하기

[`skills/voltdir/bin/`](skills/voltdir/bin)의 `voltdir.exe`는 다른 곳에서 가져온 파일이 아니라, 이 저장소의 소스에서 `cargo build --release`로 빌드한 것입니다. SHA-256 해시는 바로 옆의 `voltdir.exe.sha256`에 기록되어 있습니다. 빌드는 결정적입니다. 링커에 `/Brepro`를 지정하므로 실행 파일에는 타임스탬프도, 빌드한 컴퓨터의 경로도 들어가지 않습니다. 같은 커밋과 같은 툴체인이면 같은 바이트가 나옵니다.

공개된 실행 파일은 **Rust 1.94.0**(`x86_64-pc-windows-msvc`)으로 빌드했습니다. 직접 확인하는 방법:

```powershell
git clone https://github.com/CubeLink-oss/voltdir
cd voltdir
rustup toolchain install 1.94.0
cargo +1.94.0 build --release
(Get-FileHash target\release\voltdir.exe -Algorithm SHA256).Hash
Get-Content skills\voltdir\bin\voltdir.exe.sha256
```

두 해시가 같아야 합니다(앞의 것은 대문자로 표시됩니다). 다르다면 같은 커밋인지, `rustc +1.94.0 -V`가 1.94.0을 표시하는지 확인하세요. MSVC 링커 버전이 달라도 바이트가 바뀔 수 있습니다. 직접 빌드하지 않은 파일을 실행하고 싶지 않다면 직접 만든 `target\release\voltdir.exe`를 쓰면 됩니다. 스킬은 `PATH`에 있는 `voltdir`도 사용할 수 있습니다.

일부 보안 스캐너는 실행 파일이 들어 있는 스킬이면 일괄적으로 경고합니다. 이는 예상된 일이며, 위의 절차가 직접 확인하는 방법입니다. 실행 파일이 할 수 있는 일과 할 수 없는 일(네트워크 코드도 자격 증명 API도 없고, 가져오는 시스템 라이브러리는 다섯 개뿐)은 [`SECURITY.md`](SECURITY.md)에 적어 두었습니다.

## 저장소 구성

| 위치 | 내용 |
|---|---|
| [`skills/voltdir/`](skills/voltdir) | 에이전트용 스킬: [`SKILL.md`](skills/voltdir/SKILL.md), 전체 참고 문서 [`reference.md`](skills/voltdir/reference.md), 빌드된 `bin/voltdir.exe` |
| [`.claude-plugin/`](.claude-plugin) | Claude Code 플러그인과 마켓플레이스 설정 |
| [`benchmarks/`](benchmarks/README.md) | 재현 가능한 벤치마크 스크립트와 [결과 요약](benchmarks/RESULTS.md) |
| `src/`, `tests/` | 소스 코드와 테스트 |
| `scripts/` | CI, 라이선스 확인, 테스트 데이터 생성기 |

## 개인정보 보호

voltdir는 전적으로 사용자의 컴퓨터에서 실행됩니다. 이름, 크기, 시간, 속성만 읽으며(파일 내용은 읽지 않음), 네트워크 연결을 열지 않고, 아무것도 수집하지 않습니다. `--out`, `--save-index`, `--save-snapshot`으로 지정한 파일만 씁니다. 이 파일에는 파일 경로가 들어 있으므로 민감한 정보로 다루세요. AI 에이전트가 voltdir를 실행하면 에이전트가 출력을 읽게 되고, 그 내용은 에이전트와의 대화의 일부가 됩니다. 함께 제공되는 skill은 사용자가 직접 지정한 폴더가 아닌 한, 드라이브 전체나 개인 폴더, 사적으로 보이는 폴더를 스캔하기 전에 에이전트가 먼저 확인하도록 요청합니다. 전체 내용은 [`PRIVACY.md`](PRIVACY.md)(영어)에 있습니다. 질문과 버그 신고는 [GitHub Issues](https://github.com/CubeLink-oss/voltdir/issues)로 보내 주세요.

## 라이선스

[Apache License 2.0](LICENSE). 저작권자는 [`NOTICE`](NOTICE)에 있습니다.
