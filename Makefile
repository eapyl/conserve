PYTHON=python

lint:
	pylint -rn --output-format colorized --ignore dura_pb2.py duralib

check:
	PYTHONPATH=. $(PYTHON) -m unittest discover -v

protos:
	protoc --python_out=duralib/ proto/dura.proto

messages.pot:
	pygettext dura.py duralib/*.py

check-staged:
	[ ! -d test-git-staged ] || rm -r test-git-staged
	git checkout-index --prefix test-git-staged/ -a
	make -C test-git-staged check