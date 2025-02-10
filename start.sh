#!/bin/bash
# 用screen 来后台运行机器人
screen -S bot -d -m python /app/bot.py

cd /app/Lagrange
./Lagrange.OneBot
