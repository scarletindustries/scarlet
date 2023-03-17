build:
	mkdir -p ./bin
	v -prod ./src -o ./bin/alc
	strip ./bin/alc
