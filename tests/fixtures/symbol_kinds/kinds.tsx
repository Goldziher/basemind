declare const React: { Component: new () => object };

interface Repo<T> {
  find(id: string): T;
  size: number;
}

interface Named {
  label: string;
}

interface Store<T> extends Repo<T> {}

export abstract class Base<T> implements Repo<T>, Named {
  label = "base";
  size: number = 0;
  abstract find(id: string): T;
}

class Panel extends React.Component {
  handleClick = () => {
    const clicked = true;
    return clicked;
  };
  get = () => 1;
  set = (value: number) => value;
  get total(): number {
    return 1;
  }
  set total(value: number) {}
}

declare function greet(name: string): void;

var legacy = 1;
var legacyHandler = function () {
  var inner = 2;
  return inner;
};

class Button extends React.Component {
  onPress = () => <button>{legacy}</button>;
}
