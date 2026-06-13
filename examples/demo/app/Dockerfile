# Demo「應用」：Python HTTP 服務，每次請求對 redis 計數 +1。
# 連線目標 redis 127.0.0.1:6379——Chefer 同一 app 的服務共享網路 namespace，
# 故 db 在 localhost 即可達（內部網路），無需也不應對外開埠。
FROM python:3.12-alpine
RUN pip install --no-cache-dir "redis==5.0.8"
WORKDIR /app
COPY server.py /app/server.py
CMD ["python", "/app/server.py"]
