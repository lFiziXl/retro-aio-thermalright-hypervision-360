#!/bin/bash
set -e

echo "🏴‍☠️ Установка Retro AIO Mascot..."

# 1. Проверка и установка Rust
if ! command -v cargo &> /dev/null; then
    echo "Rust не найден. Устанавливаем..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi

# 2. Клонирование и сборка
echo "Скачиваем исходники..."
cd /tmp
rm -rf retro_aio_install
git clone https://github.com/lFiziXl/retro-aio-thermalright-hypervision-360.git retro_aio_install
cd retro_aio_install

echo "Компилируем проект (может занять пару минут)..."
cargo build --release

# 3. Перемещение бинарника
echo "Устанавливаем..."
mkdir -p ~/.local/bin
cp target/release/retro_aio ~/.local/bin/

# 4. Настройка автозапуска
echo "Настраиваем фоновый сервис..."
mkdir -p ~/.config/systemd/user
cat <<EOF > ~/.config/systemd/user/retro_aio.service
[Unit]
Description=Retro AIO LCD Panel Daemon
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/retro_aio
Restart=always
RestartSec=3
StandardOutput=null
StandardError=null

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now retro_aio.service

# 5. Уборка
rm -rf /tmp/retro_aio_install

echo "✅ Готово! Пират поселился в системе."
