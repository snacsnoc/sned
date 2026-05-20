// Sample TypeScript fixture for tree-sitter parsing tests

interface MyInterface {
  name: string;
  value: number;
}

class MyClass implements MyInterface {
  name: string;
  value: number;

  constructor(name: string, value: number) {
    this.name = name;
    this.value = value;
  }

  getName(): string {
    return this.name;
  }
}

function topLevelFunction(): void {
  const instance = new MyClass("test", 42);
  console.log(instance.getName());
}
