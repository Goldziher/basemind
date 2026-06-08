namespace GitMind.Fixtures
{
    public class Greeter
    {
        private readonly string _name;

        public Greeter(string name)
        {
            _name = name;
        }

        public string Hello()
        {
            return Greet(_name);
        }

        public string Greet(string target)
        {
            return "Hello, " + target;
        }
    }
}
