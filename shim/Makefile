CC ?= gcc
CFLAGS = -shared -fPIC -Wall -O2
LDFLAGS = -ldl

all: libmysql_shim.so

libmysql_shim.so: mysql_shim.c
	$(CC) $(CFLAGS) -o $@ $< $(LDFLAGS)

clean:
	rm -f libmysql_shim.so
