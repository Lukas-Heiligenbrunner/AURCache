clean:
  cd backend && cargo clean
  cd frontend && flutter clean

codegen:
  cd frontend && flutter pub get && flutter pub run build_runner build

format:
  cd backend && cargo fmt
  cd frontend && dart format .

lint:
  cd backend && cargo clippy
  cd frontend && flutter analyze