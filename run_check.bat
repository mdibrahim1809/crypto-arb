@echo off
REM Move to the directory that contains this batch file.
cd /d "%~dp0"

REM Ask Cargo to compile-check the entire project without producing a final executable.
cargo check
IF ERRORLEVEL 1 (
    REM Stop here when cargo check reports an error.
    echo.
    echo cargo check FAILED.
    pause
    exit /b 1
)

REM Run the simulator after the compile check succeeds.
cargo run
IF ERRORLEVEL 1 (
    REM Stop here when cargo run reports an error.
    echo.
    echo cargo run FAILED.
    pause
    exit /b 1
)

REM Keep the terminal visible so the output can be inspected.
echo.
echo Build and run completed successfully.
pause
