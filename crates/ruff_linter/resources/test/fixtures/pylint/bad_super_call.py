class Animal:
    pass


class Tree:
    pass


class CatA(Animal):
    def __init__(self):
        super(Tree, self).__init__()  # [bad-super-call]
        super(Animal, self).__init__()


class CatB(Animal):
    def __init__(self):
        super(Animal, self).__init__() # OK
